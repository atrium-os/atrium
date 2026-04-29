/*
 * event_loop.c — phase-4 milestone: kqueue-driven input event reader.
 *
 * Opens /dev/fresco0, blocks on fresco_input_wait, prints every
 * mouse/key/resize event the host pushes into the input ring.
 *
 * To produce events: focus the Fresco macOS window and move the
 * mouse / press keys / resize. Ctrl-C inside the VM to exit.
 *
 * Auto-exits after 30 s if no events arrive (so it doesn't hang
 * indefinitely in CI).
 */

#include <stdio.h>
#include <stdlib.h>
#include <signal.h>

#include "fresco.h"

static volatile int stopping = 0;

static void
on_sigint(int sig)
{
        (void)sig;
        stopping = 1;
}

static const char *
type_name(uint16_t t)
{
        switch (t) {
        case FRESCO_INPUT_KEY:           return "KEY";
        case FRESCO_INPUT_MOUSE_MOVE:    return "MOUSE_MOVE";
        case FRESCO_INPUT_MOUSE_BUTTON:  return "MOUSE_BUTTON";
        case FRESCO_INPUT_SCROLL:        return "SCROLL";
        case FRESCO_INPUT_RESIZE:        return "RESIZE";
        default:                         return "?";
        }
}

int
main(int argc, char **argv)
{
        signal(SIGINT, on_sigint);

        int max_events = (argc > 1) ? atoi(argv[1]) : 50;

        fresco_t *f = fresco_open(NULL);
        if (f == NULL) { perror("fresco_open"); return 1; }

        fresco_display_t disp;
        fresco_get_display(f, &disp);
        printf("display: %ux%u — move mouse over the Fresco window\n",
            disp.width, disp.height);
        printf("(stopping after %d events or 30 s idle)\n\n", max_events);

        int n = 0;
        for (; n < max_events && !stopping; ) {
                fresco_input_t ev;
                int rc = fresco_input_wait(f, &ev, 30000);
                if (rc < 0) { perror("fresco_input_wait"); break; }
                if (rc == 0) {
                        printf("(idle 30 s — exiting)\n");
                        break;
                }
                n++;

                switch (ev.event_type) {
                case FRESCO_INPUT_MOUSE_MOVE:
                        printf("[%3d] %s        x=%d y=%d\n",
                               n, type_name(ev.event_type),
                               ev.value_a, ev.value_b);
                        break;
                case FRESCO_INPUT_MOUSE_BUTTON:
                        printf("[%3d] %s      btn=%u %s\n",
                               n, type_name(ev.event_type), ev.code,
                               ev.value_a ? "DOWN" : "UP");
                        break;
                case FRESCO_INPUT_KEY:
                        printf("[%3d] %s             keysym=0x%x %s\n",
                               n, type_name(ev.event_type), ev.code,
                               ev.value_a ? "DOWN" : "UP");
                        break;
                case FRESCO_INPUT_SCROLL:
                        printf("[%3d] %s          dx=%d dy=%d\n",
                               n, type_name(ev.event_type),
                               ev.value_a, ev.value_b);
                        break;
                case FRESCO_INPUT_RESIZE:
                        printf("[%3d] %s          %dx%d\n",
                               n, type_name(ev.event_type),
                               ev.value_a, ev.value_b);
                        break;
                default:
                        printf("[%3d] type=%u code=%u a=%d b=%d\n",
                               n, ev.event_type, ev.code, ev.value_a, ev.value_b);
                }
        }

        fresco_close(f);
        printf("\ndrained %d event(s)\n", n);
        return 0;
}
