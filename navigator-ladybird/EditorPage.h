#pragma once

#include <LibWeb/Page/Page.h>
#include <LibWebCommon/Page/QueuedInputEvent.h>
#ifdef ATRIUM_I0_SIGN_IN
#include <LibWebCommon/HTML/NavigationPopulationRequest.h>
#endif

struct NavigatorScene;

namespace Navigator {

// A single-page client for the offline I0 fixture. Its owner drives the event
// loop and frame clock on the engine thread, with input from the assigned embed.
class EditorPage final : public Web::PageClient {
    GC_CELL(EditorPage, Web::PageClient);
    GC_DECLARE_ALLOCATOR(EditorPage);

public:
    static GC::Ref<EditorPage> create(Gfx::Palette);
    ErrorOr<void> load_fixture(StringView html, bool embedded);
#ifdef ATRIUM_I0_SIGN_IN
    Optional<URL::URL> pending_navigation;
    bool navigation_refused { false };
    void request_navigation_start(Web::HTML::LocalNavigable&, Web::NavigationTarget target, URL::URL const& url, Utf16String, Optional<Web::HTML::NavigationStartRequest> request) override
    {
        if (target != Web::NavigationTarget::TopLevel || pending_navigation.has_value()
            || !request.has_value() || request->url != url || !request->document_resource.has<Empty>()
            || request->history_handling != Web::Bindings::NavigationHistoryBehavior::Replace) {
            navigation_refused = true;
            return;
        }
        pending_navigation = url;
    }
#endif
#ifdef ATRIUM_I1_COUNTER
    void page_did_request_unload_check(Web::HTML::CrossProcessId, GC::Ref<GC::Function<void(Web::HTML::CheckIfUnloadingIsCanceledResult)>>) override;
    void page_did_request_history_operation(Web::HTML::CrossProcessId, Web::HistoryOperationParameters) override;
    void page_did_request_before_unload(String const&) override;
    ::NavigatorScene* island_scene { nullptr };
    void page_did_close() override { closed = true; }
    bool closed { false };
#endif
    Web::PageId id() const override { return Web::PageId { 1 }; }
    Web::Page& page() override { return *m_page; }
    Web::Page const& page() const override { return *m_page; }
    bool is_connection_open() const override { return true; }
    bool is_headless() const override { return true; }
    Gfx::Palette palette() const override { return m_palette; }
    Web::DevicePixelRect screen_rect() const override { return { 0, 0, 640, 360 }; }
    size_t screen_count() const override { return 1; }
    double zoom_level() const override { return 1; }
    double device_pixel_ratio() const override { return 1; }
    double device_pixels_per_css_pixel() const override { return 1; }
    Web::CSS::PreferredColorScheme preferred_color_scheme() const override { return Web::CSS::PreferredColorScheme::Light; }
    Web::CSS::PreferredContrast preferred_contrast() const override { return Web::CSS::PreferredContrast::NoPreference; }
    Web::CSS::PreferredMotion preferred_motion() const override { return Web::CSS::PreferredMotion::NoPreference; }
    Web::HTML::CrossProcessId allocate_cross_process_id() override { return m_ids.allocate(); }
    Queue<Web::QueuedInputEvent>& input_event_queue() override { return m_input; }
    void request_file(Web::FileRequest request) override { request.on_file_request_finish(Error::from_errno(EACCES)); }
    void request_frame() override { m_needs_frame = true; }
    bool take_frame_request() { return exchange(m_needs_frame, false); }
    void report_finished_handling_input_event(Web::PageId, u64, Web::EventResult) override;
    Optional<Web::EventResult> take_input_result(u64 id) { return m_results.take(id); }

private:
    explicit EditorPage(Gfx::Palette);
    void visit_edges(JS::Cell::Visitor&) override;

    Gfx::Palette m_palette;
    Web::HTML::CrossProcessIdAllocator m_ids { .namespace_id = 1 };
    Queue<Web::QueuedInputEvent> m_input;
    HashMap<u64, Web::EventResult> m_results;
    bool m_needs_frame { false };
    GC::Ref<Web::Page> m_page;
};

}
