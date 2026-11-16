use rootglass_app::encrypted_sessions::EncryptedInstanceInfo;
use rootglass_app::{EncryptedArchiveInspection, RunId, RuntimeEvent, VerificationStatus};
use rootglass_core::identity::{ProviderCapabilities, RecipientIdentity};
use rootglass_core::LifecycleState;
use std::collections::HashSet;
use std::path::PathBuf;

const MAX_CONSOLE_BYTES: usize = 80_000;
const MAX_PENDING_EVENTS: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncryptedGuest {
    pub run_id: RunId,
    pub stable_id: Option<String>,
    pub source: PathBuf,
    pub name: String,
    pub content_sha256: Option<String>,
    pub state: EncryptedGuestState,
    pub console: String,
    pub console_truncated: bool,
    pub stop_requested: bool,
    pub operation_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EncryptedGuestState {
    Starting,
    Booting,
    Running,
    Stopping,
    Stopped,
    Exited {
        exit_code: Option<i32>,
        expected: bool,
    },
    Failed {
        summary: String,
        detail: String,
    },
}

impl EncryptedGuestState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Starting => "Starting",
            Self::Booting => "Booting",
            Self::Running => "Running",
            Self::Stopping => "Stopping",
            Self::Stopped => "Stopped",
            Self::Exited { .. } => "Exited",
            Self::Failed { .. } => "Failed",
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Booting | Self::Running | Self::Stopping
        )
    }

    fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Stopped | Self::Exited { .. } | Self::Failed { .. }
        )
    }
}

#[derive(Default)]
pub struct EncryptionWorkspace {
    pub identity_capabilities: Option<ProviderCapabilities>,
    pub identities: Vec<RecipientIdentity>,
    pub identity_loading: bool,
    pub identity_error: Option<String>,
    pub copy_notice: Option<String>,
    pub archive_path: Option<PathBuf>,
    pub verification: Option<EncryptedArchiveInspection>,
    pub verification_loading: bool,
    pub verification_error: Option<String>,
    pub selected_recipient_id: Option<String>,
    pub launching: bool,
    pub launch_error: Option<String>,
    pub instances: Vec<EncryptedGuest>,
    pub focused_run: Option<RunId>,
    pub remote_instances: Vec<EncryptedInstanceInfo>,
    pub instance_refreshing: bool,
    pub instance_error: Option<String>,
    pending_remote_stops: HashSet<String>,
    pending_events: Vec<RuntimeEvent>,
}

impl EncryptionWorkspace {
    pub fn begin_identity_refresh(&mut self) -> bool {
        if self.identity_loading {
            return false;
        }
        self.identity_loading = true;
        self.identity_error = None;
        true
    }

    pub fn finish_identity_refresh(
        &mut self,
        result: Result<(ProviderCapabilities, Vec<RecipientIdentity>), String>,
    ) {
        self.identity_loading = false;
        self.copy_notice = None;
        match result {
            Ok((capabilities, identities)) => {
                self.identity_capabilities = Some(capabilities);
                self.identities = identities;
                self.identity_error = None;
                self.choose_local_recipient();
            }
            Err(error) => {
                self.identity_capabilities = None;
                self.identities.clear();
                self.identity_error = Some(error);
                self.selected_recipient_id = None;
            }
        }
    }

    pub fn select_archive(&mut self, path: PathBuf) {
        self.archive_path = Some(path);
        self.verification = None;
        self.verification_error = None;
        self.selected_recipient_id = None;
        self.launch_error = None;
    }

    pub fn begin_verification(&mut self) -> Option<PathBuf> {
        let path = self.archive_path.clone()?;
        if self.verification_loading {
            return None;
        }
        self.verification_loading = true;
        self.verification_error = None;
        self.verification = None;
        Some(path)
    }

    pub fn finish_verification(&mut self, result: Result<EncryptedArchiveInspection, String>) {
        self.verification_loading = false;
        match result {
            Ok(inspection) => {
                self.verification = Some(inspection);
                self.verification_error = None;
                self.choose_local_recipient();
            }
            Err(error) => {
                self.verification = None;
                self.verification_error = Some(error);
                self.selected_recipient_id = None;
            }
        }
    }

    pub fn select_recipient(&mut self, recipient_id: &str) {
        if self.local_recipient_available(recipient_id) {
            self.selected_recipient_id = Some(recipient_id.to_string());
        }
    }

    pub fn local_recipient_available(&self, recipient_id: &str) -> bool {
        let Some(verification) = &self.verification else {
            return false;
        };
        let Some(capabilities) = self.identity_capabilities else {
            return false;
        };
        capabilities.can_unwrap
            && verification.recipients.iter().any(|recipient| {
                recipient.recipient_id == recipient_id
                    && recipient.provider_id == capabilities.provider_id.id()
                    && recipient.wrapping_suite_id == capabilities.wrapping_suite_id.id()
                    && capabilities.satisfies(&recipient.minimum_recipient_hardware)
            })
            && self
                .identities
                .iter()
                .any(|identity| identity.recipient_id == recipient_id)
    }

    pub fn can_launch(&self) -> bool {
        if self.launching || self.identity_loading || self.verification_loading {
            return false;
        }
        let Some(verification) = &self.verification else {
            return false;
        };
        let checks_pass = [
            &verification.publisher_signature,
            &verification.recipient_envelope,
            &verification.encrypted_rootfs,
            &verification.signed_policy,
        ]
        .iter()
        .all(|check| check.status == VerificationStatus::Verified);
        if !checks_pass
            || !self
                .selected_recipient_id
                .as_deref()
                .is_some_and(|recipient| self.local_recipient_available(recipient))
        {
            return false;
        }
        let Some(limit) = verification.max_concurrent_instances else {
            return false;
        };
        let local_active = self
            .instances
            .iter()
            .filter(|instance| {
                instance.content_sha256 == verification.content_sha256 && instance.state.is_active()
            })
            .count();
        let shared_active = verification.content_sha256.as_ref().map(|digest| {
            self.remote_instances
                .iter()
                .filter(|instance| &instance.content_sha256 == digest)
                .count()
        });
        let active = shared_active.unwrap_or(local_active).max(local_active);
        active < usize::from(limit)
    }

    pub fn begin_launch(&mut self) -> Option<(PathBuf, String)> {
        if !self.can_launch() {
            return None;
        }
        let path = self.archive_path.clone()?;
        let recipient_id = self.selected_recipient_id.clone()?;
        self.launching = true;
        self.launch_error = None;
        Some((path, recipient_id))
    }

    pub fn finish_launch(
        &mut self,
        result: Result<(RunId, Option<String>), String>,
    ) -> Option<RunId> {
        self.launching = false;
        match result {
            Ok((run_id, stable_id)) => {
                let verification = self.verification.as_ref();
                let name = verification
                    .and_then(|value| value.glass_identifier.clone())
                    .unwrap_or_else(|| "Encrypted guest".to_string());
                let guest = EncryptedGuest {
                    run_id: run_id.clone(),
                    stable_id,
                    source: self.archive_path.clone().unwrap_or_default(),
                    name,
                    content_sha256: verification.and_then(|value| value.content_sha256.clone()),
                    state: EncryptedGuestState::Starting,
                    console: String::new(),
                    console_truncated: false,
                    stop_requested: false,
                    operation_error: None,
                };
                self.instances.retain(|instance| instance.run_id != run_id);
                self.instances.push(guest);
                self.focused_run = Some(run_id.clone());
                self.launch_error = None;
                let pending = std::mem::take(&mut self.pending_events);
                self.apply_events(&pending);
                Some(run_id)
            }
            Err(error) => {
                self.pending_events.clear();
                self.launch_error = Some(error);
                None
            }
        }
    }

    pub fn focused_instance(&self) -> Option<&EncryptedGuest> {
        self.focused_run.as_ref().and_then(|run| {
            self.instances
                .iter()
                .find(|instance| &instance.run_id == run)
        })
    }

    pub fn focus(&mut self, run: &RunId) {
        if self
            .instances
            .iter()
            .any(|instance| &instance.run_id == run)
        {
            self.focused_run = Some(run.clone());
        }
    }

    pub fn console_input_request(&self, text: &str) -> Option<(RunId, Vec<u8>)> {
        let instance = self.focused_instance()?;
        if instance.state != EncryptedGuestState::Running {
            return None;
        }
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(b'\n');
        Some((instance.run_id.clone(), bytes))
    }

    pub fn finish_serial_input(&mut self, run: &RunId, result: Result<(), String>) {
        if let Some(instance) = self
            .instances
            .iter_mut()
            .find(|instance| &instance.run_id == run)
        {
            instance.operation_error = result.err();
        }
    }

    pub fn begin_stop(&mut self, run: &RunId) -> bool {
        let Some(instance) = self
            .instances
            .iter_mut()
            .find(|instance| &instance.run_id == run)
        else {
            return false;
        };
        if !instance.state.is_active() || instance.stop_requested {
            return false;
        }
        instance.stop_requested = true;
        instance.operation_error = None;
        true
    }

    pub fn finish_stop(&mut self, run: &RunId, result: Result<(), String>) {
        if let Some(instance) = self
            .instances
            .iter_mut()
            .find(|instance| &instance.run_id == run)
        {
            instance.stop_requested = false;
            match result {
                Ok(()) => {
                    instance.state = EncryptedGuestState::Stopped;
                    instance.operation_error = None;
                }
                Err(error) => instance.operation_error = Some(error),
            }
        }
    }

    pub fn apply_events(&mut self, events: &[RuntimeEvent]) {
        for event in events {
            let run = match event {
                RuntimeEvent::StateChanged { run, .. }
                | RuntimeEvent::SerialChunk { run, .. }
                | RuntimeEvent::GuestReady { run, .. }
                | RuntimeEvent::Exited { run, .. }
                | RuntimeEvent::Failed { run, .. } => run,
            };
            let Some(instance_index) = self
                .instances
                .iter()
                .position(|instance| &instance.run_id == run)
            else {
                if self.launching && self.pending_events.len() < MAX_PENDING_EVENTS {
                    self.pending_events.push(event.clone());
                }
                continue;
            };
            let instance = &mut self.instances[instance_index];
            match event {
                RuntimeEvent::StateChanged { state, .. } => {
                    if !instance.state.is_terminal() {
                        instance.state = match state {
                            LifecycleState::Starting => EncryptedGuestState::Starting,
                            LifecycleState::Booting => EncryptedGuestState::Booting,
                            LifecycleState::Running => EncryptedGuestState::Running,
                            LifecycleState::Stopping => EncryptedGuestState::Stopping,
                            LifecycleState::Stopped => EncryptedGuestState::Stopped,
                            LifecycleState::Validating
                            | LifecycleState::Verifying
                            | LifecycleState::Ready => EncryptedGuestState::Starting,
                            LifecycleState::Failed => instance.state.clone(),
                        };
                    }
                }
                RuntimeEvent::SerialChunk {
                    bytes, truncated, ..
                } => append_console(instance, bytes, *truncated),
                RuntimeEvent::GuestReady { .. } => {
                    instance.state = EncryptedGuestState::Running;
                }
                RuntimeEvent::Exited {
                    exit_code,
                    expected,
                    ..
                } => {
                    instance.state = EncryptedGuestState::Exited {
                        exit_code: *exit_code,
                        expected: *expected,
                    };
                }
                RuntimeEvent::Failed {
                    summary, detail, ..
                } => {
                    instance.state = EncryptedGuestState::Failed {
                        summary: summary.clone(),
                        detail: detail.clone(),
                    };
                }
            }
        }
    }

    pub fn finish_instance_refresh(&mut self, result: Result<Vec<EncryptedInstanceInfo>, String>) {
        self.instance_refreshing = false;
        match result {
            Ok(instances) => {
                self.remote_instances = instances;
                self.instance_error = None;
            }
            Err(error) => self.instance_error = Some(error),
        }
    }

    pub fn begin_instance_refresh(&mut self) -> bool {
        if self.instance_refreshing {
            return false;
        }
        self.instance_refreshing = true;
        self.instance_error = None;
        true
    }

    pub fn begin_remote_stop(&mut self, stable_id: &str) -> bool {
        self.pending_remote_stops.insert(stable_id.to_string())
    }

    pub fn finish_remote_stop(&mut self, stable_id: &str, result: Result<(), String>) {
        self.pending_remote_stops.remove(stable_id);
        if let Err(error) = result {
            self.instance_error = Some(error);
        }
    }

    pub fn remote_stop_pending(&self, stable_id: &str) -> bool {
        self.pending_remote_stops.contains(stable_id)
    }

    pub fn active_instance_count_for_selected_archive(&self) -> usize {
        let Some(digest) = self
            .verification
            .as_ref()
            .and_then(|inspection| inspection.content_sha256.as_ref())
        else {
            return self
                .instances
                .iter()
                .filter(|instance| {
                    Some(&instance.source) == self.archive_path.as_ref()
                        && instance.state.is_active()
                })
                .count();
        };
        self.remote_instances
            .iter()
            .filter(|instance| &instance.content_sha256 == digest)
            .count()
            .max(
                self.instances
                    .iter()
                    .filter(|instance| {
                        instance.content_sha256.as_ref() == Some(digest)
                            && instance.state.is_active()
                    })
                    .count(),
            )
    }

    fn choose_local_recipient(&mut self) {
        let Some(verification) = &self.verification else {
            return;
        };
        let Some(capabilities) = self.identity_capabilities else {
            return;
        };
        let matching = verification
            .recipients
            .iter()
            .filter(|recipient| {
                recipient.provider_id == capabilities.provider_id.id()
                    && recipient.wrapping_suite_id == capabilities.wrapping_suite_id.id()
                    && capabilities.satisfies(&recipient.minimum_recipient_hardware)
                    && self
                        .identities
                        .iter()
                        .any(|identity| identity.recipient_id == recipient.recipient_id)
            })
            .map(|recipient| recipient.recipient_id.clone())
            .collect::<Vec<_>>();
        if matching.len() == 1 {
            self.selected_recipient_id = matching.into_iter().next();
        } else if self
            .selected_recipient_id
            .as_deref()
            .is_some_and(|selected| !matching.iter().any(|candidate| candidate == selected))
        {
            self.selected_recipient_id = None;
        }
    }
}

fn append_console(instance: &mut EncryptedGuest, bytes: &[u8], truncated: bool) {
    let output = String::from_utf8_lossy(bytes);
    instance.console.push_str(&output);
    instance.console_truncated |= truncated;
    if instance.console.len() > MAX_CONSOLE_BYTES {
        let mut start = instance.console.len() - MAX_CONSOLE_BYTES;
        while !instance.console.is_char_boundary(start) {
            start += 1;
        }
        instance.console.drain(..start);
        instance.console_truncated = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rootglass_app::{VerificationCheck, VerifiedRecipientInfo};
    use rootglass_core::identity::ProviderCapabilities;
    use rootglass_app::MinimumRecipientHardware;
    use rootglass_signature::{RecipientProvider, RecipientWrappingSuite};

    fn workspace() -> EncryptionWorkspace {
        let capabilities = ProviderCapabilities {
            provider_id: RecipientProvider::AgeX25519,
            wrapping_suite_id: RecipientWrappingSuite::AgeV1X25519,
            hardware_backed: false,
            can_create: true,
            can_unwrap: true,
        };
        let identity = RecipientIdentity {
            recipient_id: "age:recipient-one".to_string(),
            provider_id: RecipientProvider::AgeX25519,
            wrapping_suite_id: RecipientWrappingSuite::AgeV1X25519,
            public_key: "age1publicrecipient".to_string(),
            label: Some("Test identity".to_string()),
        };
        let verified = VerificationCheck {
            status: VerificationStatus::Verified,
            detail: "verified".to_string(),
        };
        let verification = EncryptedArchiveInspection {
            content_sha256: Some("a".repeat(64)),
            glass_identifier: Some("io.rootglass.example".to_string()),
            glass_version: Some("1.0.0".to_string()),
            publisher_signature: verified.clone(),
            recipient_envelope: verified.clone(),
            encrypted_rootfs: verified.clone(),
            signed_policy: verified,
            recipients: vec![VerifiedRecipientInfo {
                recipient_id: identity.recipient_id.clone(),
                provider_id: "age-x25519".to_string(),
                wrapping_suite_id: "age-v1-x25519".to_string(),
                minimum_recipient_hardware: MinimumRecipientHardware::SoftwareAllowed,
            }],
            max_concurrent_instances: Some(2),
            minimum_recipient_hardware: Some(MinimumRecipientHardware::SoftwareAllowed),
        };
        let mut workspace = EncryptionWorkspace::default();
        workspace.identity_capabilities = Some(capabilities);
        workspace.identities.push(identity);
        workspace.archive_path = Some(PathBuf::from("sample.glass"));
        workspace.verification = Some(verification);
        workspace.selected_recipient_id = Some("age:recipient-one".to_string());
        workspace
    }

    #[test]
    fn verified_local_identity_can_launch_only_with_all_checks_and_remaining_capacity() {
        let mut workspace = workspace();
        assert!(workspace.can_launch());
        workspace
            .verification
            .as_mut()
            .unwrap()
            .signed_policy
            .status = VerificationStatus::Failed;
        assert!(!workspace.can_launch());
        workspace
            .verification
            .as_mut()
            .unwrap()
            .signed_policy
            .status = VerificationStatus::Verified;
        for run in [RunId(1), RunId(2)] {
            workspace.instances.push(EncryptedGuest {
                run_id: run,
                stable_id: None,
                source: PathBuf::from("sample.glass"),
                name: "io.rootglass.example".to_string(),
                content_sha256: Some("a".repeat(64)),
                state: EncryptedGuestState::Running,
                console: String::new(),
                console_truncated: false,
                stop_requested: false,
                operation_error: None,
            });
        }
        assert!(!workspace.can_launch());
    }

    #[test]
    fn runtime_events_keep_console_lifecycle_and_failure_per_instance() {
        let mut workspace = workspace();
        let source = PathBuf::from("sample.glass");
        for run_id in [RunId(1), RunId(2)] {
            workspace.instances.push(EncryptedGuest {
                run_id: run_id.clone(),
                stable_id: None,
                source: source.clone(),
                name: format!("run {}", run_id.0),
                content_sha256: Some("a".repeat(64)),
                state: EncryptedGuestState::Starting,
                console: String::new(),
                console_truncated: false,
                stop_requested: false,
                operation_error: None,
            });
        }
        workspace.apply_events(&[
            RuntimeEvent::GuestReady {
                run: RunId(1),
                evidence: "ready".to_string(),
            },
            RuntimeEvent::SerialChunk {
                run: RunId(1),
                bytes: b"first instance".to_vec(),
                truncated: false,
            },
            RuntimeEvent::Failed {
                run: RunId(2),
                phase: LifecycleState::Failed,
                summary: "launch failed".to_string(),
                detail: "QMP connection refused".to_string(),
            },
        ]);
        assert_eq!(workspace.instances[0].state, EncryptedGuestState::Running);
        assert_eq!(workspace.instances[0].console, "first instance");
        assert!(matches!(
            &workspace.instances[1].state,
            EncryptedGuestState::Failed { summary, .. } if summary == "launch failed"
        ));
        assert!(workspace.instances[1].console.is_empty());
    }

    #[test]
    fn launch_applies_events_that_arrive_before_the_run_handle_is_published() {
        let mut workspace = workspace();
        workspace.begin_launch().expect("verified local recipient");
        workspace.apply_events(&[RuntimeEvent::StateChanged {
            run: RunId(12),
            state: LifecycleState::Booting,
        }]);
        workspace.finish_launch(Ok((RunId(12), Some("digest:0".to_string()))));
        assert_eq!(workspace.instances[0].state, EncryptedGuestState::Booting);
    }

    #[test]
    fn stop_errors_are_visible_and_success_marks_only_the_target_instance_stopped() {
        let mut workspace = workspace();
        let run = RunId(8);
        workspace.finish_launch(Ok((run.clone(), None)));
        assert!(workspace.begin_stop(&run));
        workspace.finish_stop(&run, Err("QMP shutdown failed".to_string()));
        assert_eq!(
            workspace.instances[0].operation_error.as_deref(),
            Some("QMP shutdown failed")
        );
        assert!(workspace.begin_stop(&run));
        workspace.finish_stop(&run, Ok(()));
        workspace.apply_events(&[RuntimeEvent::StateChanged {
            run: run.clone(),
            state: LifecycleState::Stopping,
        }]);
        assert_eq!(workspace.instances[0].state, EncryptedGuestState::Stopped);
    }
}
