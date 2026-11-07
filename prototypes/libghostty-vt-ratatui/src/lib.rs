use std::ffi::c_void;
use std::io;
use std::os::fd::RawFd;

const BUFFER_CAP: usize = 64 * 1024;

unsafe extern "C" {
    fn rgvt_new(cols: u16, rows: u16) -> *mut c_void;
    fn rgvt_free(handle: *mut c_void);
    fn rgvt_resize(handle: *mut c_void, cols: u16, rows: u16) -> i32;
    fn rgvt_write(handle: *mut c_void, bytes: *const u8, len: usize);
    fn rgvt_screen(handle: *mut c_void, out: *mut i8, out_cap: usize, out_len: *mut usize) -> i32;
    fn rgvt_screen_vt(
        handle: *mut c_void,
        out: *mut i8,
        out_cap: usize,
        out_len: *mut usize,
    ) -> i32;
    fn rgvt_first_cell_is_styled(handle: *mut c_void) -> i32;
    fn rgvt_copy_range(
        handle: *mut c_void,
        x1: u16,
        y1: u16,
        x2: u16,
        y2: u16,
        out: *mut i8,
        out_cap: usize,
        out_len: *mut usize,
    ) -> i32;
    fn rgvt_encode_left(
        handle: *mut c_void,
        out: *mut u8,
        out_cap: usize,
        out_len: *mut usize,
    ) -> i32;
    fn rgvt_encode_paste(
        text: *const i8,
        text_len: usize,
        bracketed: bool,
        out: *mut u8,
        out_cap: usize,
        out_len: *mut usize,
    ) -> i32;
}

pub struct GhosttyTerminal(*mut c_void);

impl GhosttyTerminal {
    pub fn new(cols: u16, rows: u16) -> io::Result<Self> {
        let handle = unsafe { rgvt_new(cols, rows) };
        if handle.is_null() {
            return Err(io::Error::other("libghostty-vt terminal creation failed"));
        }
        Ok(Self(handle))
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        let result = unsafe { rgvt_resize(self.0, cols, rows) };
        (result == 0)
            .then_some(())
            .ok_or_else(|| io::Error::other(format!("libghostty-vt resize failed: {result}")))
    }

    pub fn write(&mut self, bytes: &[u8]) {
        unsafe { rgvt_write(self.0, bytes.as_ptr(), bytes.len()) };
    }

    pub fn screen(&self) -> io::Result<String> {
        format_buffer(|out, cap, len| unsafe { rgvt_screen(self.0, out, cap, len) })
    }

    pub fn screen_vt(&self) -> io::Result<Vec<u8>> {
        format_buffer(|out, cap, len| unsafe { rgvt_screen_vt(self.0, out, cap, len) })
            .map(String::into_bytes)
    }

    pub fn first_cell_is_styled(&self) -> io::Result<bool> {
        match unsafe { rgvt_first_cell_is_styled(self.0) } {
            0 => Ok(false),
            1 => Ok(true),
            result => Err(io::Error::other(format!(
                "libghostty-vt render-state query failed: {result}"
            ))),
        }
    }

    pub fn copy_range(&self, x1: u16, y1: u16, x2: u16, y2: u16) -> io::Result<String> {
        format_buffer(|out, cap, len| unsafe {
            rgvt_copy_range(self.0, x1, y1, x2, y2, out, cap, len)
        })
    }

    pub fn encode_left(&self) -> io::Result<Vec<u8>> {
        encode_buffer(|out, cap, len| unsafe { rgvt_encode_left(self.0, out, cap, len) })
    }
}

impl Drop for GhosttyTerminal {
    fn drop(&mut self) {
        unsafe { rgvt_free(self.0) };
    }
}

fn format_buffer(mut format: impl FnMut(*mut i8, usize, *mut usize) -> i32) -> io::Result<String> {
    let mut bytes = vec![0i8; BUFFER_CAP];
    let mut len = 0;
    let result = format(bytes.as_mut_ptr(), bytes.len(), &mut len);
    if result != 0 {
        return Err(io::Error::other(format!(
            "libghostty-vt formatter failed: {result}"
        )));
    }
    let bytes = unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<u8>(), len) };
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

fn encode_buffer(mut encode: impl FnMut(*mut u8, usize, *mut usize) -> i32) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0u8; 128];
    let mut len = 0;
    let result = encode(bytes.as_mut_ptr(), bytes.len(), &mut len);
    if result != 0 {
        return Err(io::Error::other(format!(
            "libghostty-vt input encoder failed: {result}"
        )));
    }
    bytes.truncate(len);
    Ok(bytes)
}

pub fn encode_bracketed_paste(text: &str) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0u8; text.len() + 32];
    let mut len = 0;
    let result = unsafe {
        rgvt_encode_paste(
            text.as_ptr().cast(),
            text.len(),
            true,
            bytes.as_mut_ptr(),
            bytes.len(),
            &mut len,
        )
    };
    if result != 0 {
        return Err(io::Error::other(format!(
            "libghostty-vt paste encoder failed: {result}"
        )));
    }
    bytes.truncate(len);
    Ok(bytes)
}

pub fn write_all(fd: RawFd, bytes: &[u8]) -> io::Result<()> {
    let mut written = 0;
    while written < bytes.len() {
        let result =
            unsafe { libc::write(fd, bytes[written..].as_ptr().cast(), bytes.len() - written) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if result == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "PTY accepted zero bytes",
            ));
        }
        written += result as usize;
    }
    Ok(())
}
