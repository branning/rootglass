#include <ghostty/vt.h>

#include <stdlib.h>
#include <string.h>

typedef struct {
  GhosttyTerminal terminal;
  uint8_t *pty_output;
  size_t pty_output_len;
  size_t pty_output_cap;
  bool pty_output_failed;
} RgGhosttyTerminal;

static void write_pty_reply(GhosttyTerminal terminal, void *userdata,
                            const uint8_t *data, size_t len) {
  (void)terminal;
  RgGhosttyTerminal *wrapper = userdata;
  if (!wrapper || !data || len == 0 || wrapper->pty_output_failed) return;
  if (len > SIZE_MAX - wrapper->pty_output_len) {
    wrapper->pty_output_failed = true;
    return;
  }

  size_t needed = wrapper->pty_output_len + len;
  if (needed > wrapper->pty_output_cap) {
    size_t capacity = wrapper->pty_output_cap ? wrapper->pty_output_cap : 256;
    while (capacity < needed) {
      if (capacity > SIZE_MAX / 2) {
        capacity = needed;
        break;
      }
      capacity *= 2;
    }
    uint8_t *output = realloc(wrapper->pty_output, capacity);
    if (!output) {
      wrapper->pty_output_failed = true;
      return;
    }
    wrapper->pty_output = output;
    wrapper->pty_output_cap = capacity;
  }

  memcpy(wrapper->pty_output + wrapper->pty_output_len, data, len);
  wrapper->pty_output_len = needed;
}

static int format_terminal(GhosttyTerminal terminal, int as_vt, int x1, int y1,
                           int x2, int y2, char *out, size_t out_cap,
                           size_t *out_len) {
  GhosttyFormatter formatter = NULL;
  GhosttyResult result = ghostty_formatter_terminal_new(NULL, &formatter, terminal);
  if (result != GHOSTTY_SUCCESS) return (int)result;

  bool trim = true;
  bool unwrap = true;
  result = ghostty_formatter_set(formatter, GHOSTTY_FORMATTER_OPT_TRIM, &trim);
  if (result == GHOSTTY_SUCCESS) {
    result = ghostty_formatter_set(formatter, GHOSTTY_FORMATTER_OPT_UNWRAP, &unwrap);
  }
  if (result == GHOSTTY_SUCCESS && as_vt) {
    GhosttyFormatterFormat format = GHOSTTY_FORMATTER_FORMAT_VT;
    result = ghostty_formatter_set(formatter, GHOSTTY_FORMATTER_OPT_EMIT, &format);
  }
  if (result == GHOSTTY_SUCCESS && x1 >= 0) {
    GhosttyPoint start = {
      .tag = GHOSTTY_POINT_TAG_VIEWPORT,
      .value = { .coordinate = { .x = (uint16_t)x1, .y = (uint32_t)y1 } },
    };
    GhosttyPoint end = {
      .tag = GHOSTTY_POINT_TAG_VIEWPORT,
      .value = { .coordinate = { .x = (uint16_t)x2, .y = (uint32_t)y2 } },
    };
    GhosttySelection selection = {0};
    result = ghostty_terminal_grid_ref(terminal, start, &selection.start);
    if (result == GHOSTTY_SUCCESS) {
      result = ghostty_terminal_grid_ref(terminal, end, &selection.end);
    }
    if (result == GHOSTTY_SUCCESS) {
      result = ghostty_formatter_set(formatter, GHOSTTY_FORMATTER_OPT_SELECTION,
                                     &selection);
    }
  }

  size_t needed = 0;
  if (result == GHOSTTY_SUCCESS) {
    result = ghostty_formatter_format_buf(formatter, NULL, 0, &needed);
    if (result == GHOSTTY_OUT_OF_SPACE) result = GHOSTTY_SUCCESS;
  }
  if (result == GHOSTTY_SUCCESS) {
    *out_len = needed;
    if (needed > out_cap) {
      result = GHOSTTY_OUT_OF_SPACE;
    } else if (needed > 0) {
      result = ghostty_formatter_format_buf(formatter, (uint8_t *)out,
                                            out_cap, out_len);
    }
  }
  ghostty_formatter_free(formatter);
  return (int)result;
}

void *rgvt_new(uint16_t cols, uint16_t rows) {
  RgGhosttyTerminal *wrapper = calloc(1, sizeof(*wrapper));
  if (!wrapper) return NULL;
  if (ghostty_terminal_new(NULL, &wrapper->terminal, cols, rows) != GHOSTTY_SUCCESS) {
    free(wrapper);
    return NULL;
  }
  if (ghostty_terminal_set(wrapper->terminal, GHOSTTY_TERMINAL_OPT_USERDATA,
                           wrapper) != GHOSTTY_SUCCESS ||
      ghostty_terminal_set(wrapper->terminal, GHOSTTY_TERMINAL_OPT_WRITE_PTY,
                           (const void *)write_pty_reply) != GHOSTTY_SUCCESS) {
    ghostty_terminal_free(wrapper->terminal);
    free(wrapper);
    return NULL;
  }
  return wrapper;
}

void rgvt_free(void *handle) {
  RgGhosttyTerminal *wrapper = handle;
  if (!wrapper) return;
  ghostty_terminal_free(wrapper->terminal);
  free(wrapper->pty_output);
  free(wrapper);
}

int rgvt_resize(void *handle, uint16_t cols, uint16_t rows) {
  RgGhosttyTerminal *wrapper = handle;
  if (!wrapper) return (int)GHOSTTY_INVALID_VALUE;
  return (int)ghostty_terminal_resize(wrapper->terminal, cols, rows, 8, 16);
}

void rgvt_write(void *handle, const uint8_t *bytes, size_t len) {
  RgGhosttyTerminal *wrapper = handle;
  if (wrapper && bytes && len) {
    ghostty_terminal_vt_write(wrapper->terminal, bytes, len);
  }
}

int rgvt_take_pty_output(void *handle, uint8_t *out, size_t out_cap,
                         size_t *out_len) {
  RgGhosttyTerminal *wrapper = handle;
  if (!wrapper || !out_len) return (int)GHOSTTY_INVALID_VALUE;
  if (wrapper->pty_output_failed) return (int)GHOSTTY_OUT_OF_MEMORY;

  *out_len = wrapper->pty_output_len;
  if (wrapper->pty_output_len > out_cap) return (int)GHOSTTY_OUT_OF_SPACE;
  if (wrapper->pty_output_len > 0 && !out) return (int)GHOSTTY_INVALID_VALUE;
  if (wrapper->pty_output_len > 0) {
    memcpy(out, wrapper->pty_output, wrapper->pty_output_len);
  }
  wrapper->pty_output_len = 0;
  return (int)GHOSTTY_SUCCESS;
}

int rgvt_screen(void *handle, char *out, size_t out_cap, size_t *out_len) {
  RgGhosttyTerminal *wrapper = handle;
  if (!wrapper || !out_len) return (int)GHOSTTY_INVALID_VALUE;
  return format_terminal(wrapper->terminal, 0, -1, 0, -1, 0, out, out_cap,
                         out_len);
}

int rgvt_screen_vt(void *handle, char *out, size_t out_cap, size_t *out_len) {
  RgGhosttyTerminal *wrapper = handle;
  if (!wrapper || !out_len) return (int)GHOSTTY_INVALID_VALUE;
  return format_terminal(wrapper->terminal, 1, -1, 0, -1, 0, out, out_cap,
                         out_len);
}

int rgvt_first_cell_is_styled(void *handle) {
  RgGhosttyTerminal *wrapper = handle;
  if (!wrapper) return -1;

  GhosttyRenderState state = NULL;
  GhosttyRenderStateRowIterator rows = NULL;
  GhosttyRenderStateRowCells cells = NULL;
  GhosttyResult result = ghostty_render_state_new(NULL, &state);
  if (result == GHOSTTY_SUCCESS) {
    result = ghostty_render_state_update(state, wrapper->terminal);
  }
  if (result == GHOSTTY_SUCCESS) {
    result = ghostty_render_state_row_iterator_new(NULL, &rows);
  }
  if (result == GHOSTTY_SUCCESS) {
    result = ghostty_render_state_row_cells_new(NULL, &cells);
  }
  if (result == GHOSTTY_SUCCESS) {
    result = ghostty_render_state_get(state, GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                                      &rows);
  }

  int styled = -1;
  while (result == GHOSTTY_SUCCESS && ghostty_render_state_row_iterator_next(rows)) {
    int32_t y = -1;
    result = ghostty_render_state_row_get(rows, GHOSTTY_RENDER_STATE_ROW_DATA_VIEWPORT_Y,
                                          &y);
    if (result != GHOSTTY_SUCCESS || y != 0) continue;
    result = ghostty_render_state_row_get(rows, GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                                          &cells);
    if (result == GHOSTTY_SUCCESS) {
      result = ghostty_render_state_row_cells_select(cells, 0);
    }
    if (result == GHOSTTY_SUCCESS) {
      bool has_styling = false;
      result = ghostty_render_state_row_cells_get(
          cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_HAS_STYLING, &has_styling);
      if (result == GHOSTTY_SUCCESS) styled = has_styling ? 1 : 0;
    }
    break;
  }

  ghostty_render_state_row_cells_free(cells);
  ghostty_render_state_row_iterator_free(rows);
  ghostty_render_state_free(state);
  return result == GHOSTTY_SUCCESS ? styled : -1;
}

int rgvt_copy_range(void *handle, uint16_t x1, uint16_t y1, uint16_t x2,
                    uint16_t y2, char *out, size_t out_cap, size_t *out_len) {
  RgGhosttyTerminal *wrapper = handle;
  if (!wrapper || !out_len) return (int)GHOSTTY_INVALID_VALUE;
  return format_terminal(wrapper->terminal, 0, x1, y1, x2, y2, out, out_cap,
                         out_len);
}

int rgvt_encode_left(void *handle, uint8_t *out, size_t out_cap,
                     size_t *out_len) {
  RgGhosttyTerminal *wrapper = handle;
  if (!wrapper || !out_len) return (int)GHOSTTY_INVALID_VALUE;

  GhosttyKeyEncoder encoder = NULL;
  GhosttyKeyEvent event = NULL;
  GhosttyResult result = ghostty_key_encoder_new(NULL, &encoder);
  if (result == GHOSTTY_SUCCESS) {
    result = ghostty_key_event_new(NULL, &event);
  }
  if (result == GHOSTTY_SUCCESS) {
    result = ghostty_key_encoder_set_from_terminal(encoder, wrapper->terminal);
  }
  if (result == GHOSTTY_SUCCESS) {
    result = ghostty_key_event_set_action(event, GHOSTTY_KEY_ACTION_PRESS);
  }
  if (result == GHOSTTY_SUCCESS) {
    result = ghostty_key_event_set_key(event, GHOSTTY_KEY_ARROW_LEFT);
  }
  if (result == GHOSTTY_SUCCESS) {
    result = ghostty_key_encoder_encode(encoder, event, out, out_cap, out_len);
  }
  ghostty_key_event_free(event);
  ghostty_key_encoder_free(encoder);
  return (int)result;
}

int rgvt_encode_paste(const char *text, size_t text_len, bool bracketed,
                      uint8_t *out, size_t out_cap, size_t *out_len) {
  if (!text || !out_len) return (int)GHOSTTY_INVALID_VALUE;
  char *mutable_text = malloc(text_len ? text_len : 1);
  if (!mutable_text) return (int)GHOSTTY_OUT_OF_MEMORY;
  memcpy(mutable_text, text, text_len);
  GhosttyResult result = ghostty_paste_encode(mutable_text, text_len, bracketed,
                                               out, out_cap, out_len);
  free(mutable_text);
  return (int)result;
}
