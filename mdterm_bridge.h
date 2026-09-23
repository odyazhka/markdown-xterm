/*
 * mdterm_bridge.h
 *
 * C-facing API for the Rust markdown -> ANSI streaming converter.
 * Drop this next to ptydata.c (or anywhere on xterm's include path)
 * and #include it from ptydata.c - see xterm-411-mdterm.patch.
 */
#ifndef MDTERM_BRIDGE_H
#define MDTERM_BRIDGE_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/*
 * Create a new converter instance. Holds the "incomplete line so far"
 * buffer plus whether we're currently inside a ``` code fence.
 * Returns an opaque handle; never returns NULL (aborts the process on
 * allocation failure, same as C's malloc-or-die idiom).
 */
void *mdterm_new(void);

/*
 * Feed a raw chunk of bytes exactly as read() returned them from the
 * PTY (already parity-stripped is fine, mdterm doesn't care).
 *
 * The converter buffers input internally until it has at least one
 * complete line (terminated by '\n'). Any complete lines are scanned
 * for markdown (headings, **bold**, *italic*, `code`, fenced code
 * blocks, "- " list bullets, "> " blockquotes, --- rules) and
 * rewritten with ANSI SGR escape sequences in place of the markdown
 * syntax. Anything left over that doesn't end in '\n' yet is held
 * back internally and prepended to the next call's input.
 *
 * Return value:
 *   - non-NULL: *out_len holds the length of the returned buffer.
 *     The caller now owns it and MUST release it via
 *     mdterm_free_buf() once done.
 *   - NULL: *out_len is set to 0. This means no complete line is
 *     available yet - the caller should go back and read() more
 *     from the PTY rather than treating this as an error or EOF.
 */
unsigned char *mdterm_feed(void *state,
                            const unsigned char *input,
                            size_t input_len,
                            size_t *out_len);

/* Number of bytes currently held back (a partial line with no '\n' yet). */
size_t mdterm_pending_len(const void *state);

/*
 * Take the held-back partial line back *unconverted* and clear it.
 * The C side calls this when no '\n' has arrived for a short while
 * (shell prompts, echoed keystrokes) or before switching to raw mode.
 * Returns NULL and sets *out_len = 0 if nothing is pending; otherwise
 * release the buffer with mdterm_free_buf().
 */
unsigned char *mdterm_flush(void *state, size_t *out_len);

/* Free a buffer previously returned by mdterm_feed() / mdterm_flush(). Safe to call
 * with ptr == NULL (no-op). */
void mdterm_free_buf(unsigned char *ptr, size_t len);

/* Destroy a converter instance created with mdterm_new(). Safe to
 * call with state == NULL (no-op). Not called anywhere in the
 * xterm patch since xterm just exits and the OS reclaims
 * everything, but it's here for embedding elsewhere / tests. */
void mdterm_free(void *state);

#ifdef __cplusplus
}
#endif

#endif /* MDTERM_BRIDGE_H */
