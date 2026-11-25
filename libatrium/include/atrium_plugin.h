/* atrium_plugin.h -- write an Artifex plugin in C (dev-tools.md §3.8).
 *
 * A plugin is an app whose atrium.toml implements `editor-extension`:
 *
 *   [roles]
 *   implements = ["editor-extension"]
 *
 *   [roles.editor-extension]
 *   languages = ["c"]            # "*" for all
 *   commands = ["hello.count"]   # shown in Artifex's command palette
 *   panel = "Hello"              # if it draws one
 *   workspace-read = false       # true to ask to read the workspace
 *
 * limend starts it in its own jail when Artifex opens a file of one of its
 * languages. It speaks the Language Server Protocol with Artifex: each
 * ATRIUM_PLUGIN_EV_LSP event is one LSP message as JSON text, and it
 * answers as a language server would (textDocument/publishDiagnostics,
 * completion, code actions...). Files are artifex:///<path in the
 * workspace>; their whole text comes in didOpen and didChange. Treat it as
 * hostile input.
 *
 *   atrium_plugin_t *p;
 *   if (atrium_plugin_start(&p) != 0) return 1;
 *   atrium_plugin_register_command(p, "hello.count", "Count the lines");
 *   for (;;) {
 *       atrium_plugin_event_t ev;
 *       int r = atrium_plugin_next(p, 1000, &ev);
 *       if (r < 0 || (r == 1 && ev.kind == ATRIUM_PLUGIN_EV_DETACHING)) break;
 *       ...
 *   }
 *   atrium_plugin_free(p);
 *
 * Link with -latrium (pkg-config --cflags --libs atrium). A handle is used
 * from one thread at a time. Strings the library hands out stay valid
 * until the next atrium_plugin_next or atrium_plugin_free on that handle;
 * copy what you keep.
 */

#ifndef ATRIUM_PLUGIN_H
#define ATRIUM_PLUGIN_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Errors: every call returns 0 (or a count, an id) on success. */
#define ATRIUM_PLUGIN_ERR_CHANNEL       -60  /* limend's channel failed */
#define ATRIUM_PLUGIN_ERR_CLOSED        -61  /* limend closed it: exit */
#define ATRIUM_PLUGIN_ERR_PROTOCOL      -62  /* out of shape, or over 4 MiB */
#define ATRIUM_PLUGIN_ERR_INVALID       -63  /* NULL, not UTF-8, not JSON */
#define ATRIUM_PLUGIN_ERR_NO_PANEL      -64  /* no frescod connection/token */
#define ATRIUM_PLUGIN_ERR_BUF_TOO_SMALL -65

/* Event kinds. */
#define ATRIUM_PLUGIN_EV_LSP          1u  /* json, json_len */
#define ATRIUM_PLUGIN_EV_COMMAND      2u  /* command, file, line */
#define ATRIUM_PLUGIN_EV_PANEL_SHOWN  3u  /* shown */
#define ATRIUM_PLUGIN_EV_DETACHING    4u  /* Artifex closed it: exit */

typedef struct atrium_plugin atrium_plugin_t;

typedef struct {
    uint32_t    kind;
    uint32_t    line;      /* COMMAND: the caret's line, from 0 */
    int32_t     shown;     /* PANEL_SHOWN: 1 shown, 0 hidden */
    int32_t     _pad;
    const char *json;      /* LSP: one whole message, NUL-terminated */
    size_t      json_len;
    const char *command;   /* COMMAND: its id */
    const char *file;      /* COMMAND: the active file's URI, or NULL */
} atrium_plugin_event_t;

/* Start as limend's child: wait (up to 30 s) to be attached and for
 * Artifex's first message. */
int32_t atrium_plugin_start(atrium_plugin_t **out);
void    atrium_plugin_free(atrium_plugin_t *p);

/* The workspace's name, for display. */
const char *atrium_plugin_workspace_name(atrium_plugin_t *p);
/* "/work" (read-only) when the person allowed this plugin to read the
 * workspace (manifest workspace-read = true); NULL otherwise. */
const char *atrium_plugin_workspace_dir(atrium_plugin_t *p);
/* The i-th of its languages Artifex has files of open; NULL past the end. */
const char *atrium_plugin_language(atrium_plugin_t *p, size_t i);

/* The next thing Artifex sent, waiting at most wait_ms (up to a minute).
 * 1 with *out filled, 0 when nothing came, negative on an error. */
int32_t atrium_plugin_next(atrium_plugin_t *p, uint32_t wait_ms, atrium_plugin_event_t *out);

/* LSP out. msg: one whole message, a JSON object. */
int32_t atrium_plugin_send(atrium_plugin_t *p, const char *msg);
/* The answer to a request: id_json is its "id" as JSON (a number or a
 * string, as it came); result_json its result (NULL: null). */
int32_t atrium_plugin_reply(atrium_plugin_t *p, const char *id_json, const char *result_json);
/* A notification; params_json NULL for none. */
int32_t atrium_plugin_notify(atrium_plugin_t *p, const char *method, const char *params_json);

/* A command in Artifex's palette; id must be in the manifest's commands
 * (a-z 0-9 . - _). Running it gives ATRIUM_PLUGIN_EV_COMMAND. */
int32_t atrium_plugin_register_command(atrium_plugin_t *p, const char *id, const char *label);
/* A line in Artifex's status bar (cut to 120 bytes; "" clears it). */
int32_t atrium_plugin_status(atrium_plugin_t *p, const char *line);

/* The panel (manifest panel): opens a width x height window and asks
 * Artifex to show it as the tab `title`. Returns the window id; draw it
 * with atrium.h's atrium_window_frame_* calls. Calling it again shows the
 * same window again. */
int32_t atrium_plugin_open_panel(atrium_plugin_t *p, const char *title, uint32_t width, uint32_t height);
int32_t atrium_plugin_close_panel(atrium_plugin_t *p);

/* The workspace path an artifex:/// URI names, NUL-terminated into buf.
 * Returns its length; ATRIUM_PLUGIN_ERR_INVALID for any other URI or one
 * that climbs out with "..". */
int32_t atrium_plugin_path_of(const char *uri, char *buf, size_t len);

/* Read a field of a JSON message (an event's json): path is dot-separated
 * members and array indexes, e.g. "params.textDocument.uri" or
 * "params.contentChanges.0.text" ("" for the whole message). With raw 0 a
 * string comes decoded, anything else as JSON text; with raw 1 everything
 * comes as JSON text (a string keeps its quotes and escapes, ready to put
 * back into a message, as an "id" must be). Writes into buf, NUL-terminated
 * and cut to fit, and returns the whole value's length as snprintf does:
 * a return >= len means it was cut. ATRIUM_PLUGIN_ERR_INVALID when nothing
 * is at path (or a decoded string holds a NUL). */
int32_t atrium_plugin_json_get(const char *json, const char *path, int raw, char *buf, size_t len);

#ifdef __cplusplus
}
#endif

#endif /* ATRIUM_PLUGIN_H */
