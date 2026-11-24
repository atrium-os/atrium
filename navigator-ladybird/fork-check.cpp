// Offline I5 gate. All children run in the probe's jail. No site channel is opened.
#include "CheckConfinement.h"
#include "EditorPage.h"
#include "PinnedFonts.h"
#include <LibCore/EventLoop.h>
#include <LibGfx/SystemTheme.h>
#include <LibWeb/Bindings/MainThreadVM.h>
#include <LibWeb/Platform/EventLoopPlugin.h>
#include <sys/wait.h>
#include <time.h>

extern "C" int __isthreaded; // FreeBSD libc: set once libthr starts another thread.

static constexpr char editor[] = {
#embed "editor.html"
};
static unsigned long long now_us()
{
    timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000000ULL + ts.tv_nsec / 1000;
}

static unsigned long long first_scene;
extern "C" bool __real_navigator_scene_sample(NavigatorScene*, Web::Layout::RustFFI::DocumentHost*, bool, bool);
extern "C" bool __wrap_navigator_scene_sample(NavigatorScene* scene, Web::Layout::RustFFI::DocumentHost* host, bool visible, bool blink)
{
    bool ok = __real_navigator_scene_sample(scene, host, visible, blink);
    if (ok && !first_scene) first_scene = now_us();
    return ok;
}

static int extra_fds(int limit)
{
    int count = 0;
    for (int fd = 3; fd < limit; ++fd) {
        if (fcntl(fd, F_GETFD) >= 0) ++count;
        else if (errno != EBADF) return -1;
    }
    return count;
}

static int render()
{
    Core::EventLoop loop;
    Web::Platform::EventLoopPlugin::install(*new Web::Platform::EventLoopPlugin);
    Web::Bindings::initialize_agent(Web::HTML::AgentType::SimilarOriginWindow);
    auto theme = Core::AnonymousBuffer::create_with_size(sizeof(Gfx::SystemTheme));
    if (theme.is_error()) return 1;
    auto client = Navigator::EditorPage::create(Gfx::Palette(Gfx::PaletteImpl::create_with_anonymous_buffer(theme.release_value())));
    auto result = client->load_fixture({ editor, sizeof(editor) }, false);
    if (result.is_error()) {
        warnln("fork-check editor: {}", result.error());
        return 1;
    }
    return 0;
}

static bool trials(bool fonts_ready)
{
    static volatile pid_t child_mark;
    for (int trial = 0; trial < 3; ++trial) {
        if (__isthreaded || child_mark != 0) return false;
        fflush(nullptr);
        auto start = now_us();
        auto child = fork();
        if (child == 0) {
            unsigned mode;
            if (cap_getmode(&mode) || mode != 1 || getuid() == 0 || child_mark != 0) _exit(1);
            child_mark = getpid();
            auto fork_us = now_us() - start;
            if (!fonts_ready && Navigator::install_pinned_fonts().is_error()) _exit(1);
            alarm(20);
            int status = render();
            if (!first_scene) _exit(1);
            printf("fork-check fonts=%d trial=%d pid=%d fork_us=%llu first_scene_us=%llu editor_us=%llu status=%d\n",
                fonts_ready, trial, getpid(), fork_us, first_scene - start, now_us() - start, status);
            fflush(nullptr);
            _exit(status);
        }
        if (child < 0) { perror("fork"); return false; }
        int status;
        if (waitpid(child, &status, 0) != child) { perror("waitpid"); return false; }
        if (!WIFEXITED(status) || WEXITSTATUS(status) || child_mark != 0) {
            fprintf(stderr, "fork-check child=%d wait_status=%d mark=%d\n", child, status, int(child_mark));
            return false;
        }
    }
    return true;
}

int main()
{
    alarm(90);
    int limit = getdtablesize();
    if (limit < 3) return 1;
    closefrom(3);
    if (!Navigator::enter_fixture_capability_mode()) return 1;
    if (extra_fds(limit) != 0 || !trials(false)) return 2;
    auto start = now_us();
    if (Navigator::install_pinned_fonts().is_error()) return 3;
    printf("fork-check template=%d fonts_us=%llu threaded=%d\n", getpid(), now_us() - start, __isthreaded);
    int font_fds = extra_fds(limit);
    printf("fork-check fonts extra_fds=%d fd_limit=%d\n", font_fds, limit);
    if (font_fds != 0 || !trials(true)) return 4;
    // Inventory later stages without forking an event loop or a threaded runtime.
    Core::EventLoop loop;
    printf("fork-check event-loop threaded=%d extra_fds=%d\n", __isthreaded, extra_fds(limit));
    Web::Platform::EventLoopPlugin::install(*new Web::Platform::EventLoopPlugin);
    Web::Bindings::initialize_agent(Web::HTML::AgentType::SimilarOriginWindow);
    printf("fork-check agent threaded=%d extra_fds=%d\n", __isthreaded, extra_fds(limit));
    fflush(nullptr);
    _exit(0);
}
