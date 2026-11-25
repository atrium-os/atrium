/* words: a sample Artifex plugin in C (dev-tools.md §3.8).
 *
 * Artifex sends each open Markdown file's whole text in didOpen and
 * didChange; the plugin keeps a count of its words and lines, and the
 * palette command "words.count" puts the active file's in the status bar.
 * Everything it reads is the person's file text: it is only counted.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <atrium_plugin.h>

#define MAX_FILES 64
#define URI_MAX   1024

struct count {
    char   uri[URI_MAX];
    size_t words, lines;
};

static struct count files[MAX_FILES];
static size_t nfiles;

static struct count *
find(const char *uri, int add)
{
    for (size_t i = 0; i < nfiles; i++)
        if (strcmp(files[i].uri, uri) == 0)
            return &files[i];
    if (!add)
        return NULL;
    /* Full: reuse the first slot. */
    struct count *c = nfiles < MAX_FILES ? &files[nfiles++] : &files[0];
    snprintf(c->uri, sizeof(c->uri), "%s", uri);
    return c;
}

static void
tally(struct count *c, const char *text)
{
    int in_word = 0;
    c->words = 0;
    c->lines = *text ? 1 : 0;
    for (const char *p = text; *p; p++) {
        if (*p == '\n' && p[1])
            c->lines++;
        int space = *p == ' ' || *p == '\n' || *p == '\t' || *p == '\r';
        if (!space && !in_word)
            c->words++;
        in_word = !space;
    }
}

/* A string field of msg, malloc'd (NULL if it isn't there). */
static char *
field(const char *msg, const char *path, int raw)
{
    int n = atrium_plugin_json_get(msg, path, raw, NULL, 0);
    if (n < 0)
        return NULL;
    char *s = malloc((size_t)n + 1);
    if (s != NULL)
        atrium_plugin_json_get(msg, path, raw, s, (size_t)n + 1);
    return s;
}

static int
lsp(atrium_plugin_t *p, const char *msg)
{
    char *method = field(msg, "method", 0);
    char *id = field(msg, "id", 1);
    int r = 0;
    if (method == NULL) {
        /* An answer to something it never asked: nothing to do. */
    } else if (strcmp(method, "textDocument/didOpen") == 0 ||
               strcmp(method, "textDocument/didChange") == 0) {
        char *uri = field(msg, "params.textDocument.uri", 0);
        char *text = field(msg, strcmp(method, "textDocument/didOpen") == 0 ?
            "params.textDocument.text" : "params.contentChanges.0.text", 0);
        if (uri != NULL && text != NULL && strlen(uri) < URI_MAX)
            tally(find(uri, 1), text);
        free(uri);
        free(text);
    } else if (strcmp(method, "initialized") == 0) {
        r = atrium_plugin_register_command(p, "words.count", "Count the words in this file");
    } else if (id != NULL && strcmp(method, "initialize") == 0) {
        r = atrium_plugin_reply(p, id,
            "{\"capabilities\":{\"textDocumentSync\":1},\"serverInfo\":{\"name\":\"words\"}}");
    } else if (id != NULL) {
        r = atrium_plugin_reply(p, id, NULL);
    }
    free(method);
    free(id);
    return r;
}

static int
command(atrium_plugin_t *p, const atrium_plugin_event_t *ev)
{
    char line[160], path[URI_MAX];
    struct count *c;
    if (strcmp(ev->command, "words.count") != 0)
        return 0;
    if (ev->file == NULL || (c = find(ev->file, 0)) == NULL)
        return atrium_plugin_status(p, "Open a Markdown file to count its words");
    if (atrium_plugin_path_of(ev->file, path, sizeof(path)) < 0)
        snprintf(path, sizeof(path), "this file");
    snprintf(line, sizeof(line), "%zu word%s, %zu line%s in %.80s",
        c->words, c->words == 1 ? "" : "s", c->lines, c->lines == 1 ? "" : "s", path);
    return atrium_plugin_status(p, line);
}

int
main(void)
{
    atrium_plugin_t *p;
    int r = atrium_plugin_start(&p);
    if (r != 0) {
        fprintf(stderr, "words: no session (%d)\n", r);
        return 1;
    }
    for (;;) {
        atrium_plugin_event_t ev;
        r = atrium_plugin_next(p, 1000, &ev);
        if (r == 0)
            continue;
        if (r < 0 || ev.kind == ATRIUM_PLUGIN_EV_DETACHING)
            break;
        if (ev.kind == ATRIUM_PLUGIN_EV_LSP)
            r = lsp(p, ev.json);
        else if (ev.kind == ATRIUM_PLUGIN_EV_COMMAND)
            r = command(p, &ev);
        if (r < 0)
            break;
    }
    atrium_plugin_free(p);
    if (r < 0 && r != ATRIUM_PLUGIN_ERR_CLOSED) {
        fprintf(stderr, "words: %d\n", r);
        return 1;
    }
    return 0;
}
