#pragma once

#include <LibWeb/Page/Page.h>
#include <LibWebCommon/Page/QueuedInputEvent.h>
#if defined(ATRIUM_I0_SIGN_IN) || defined(ATRIUM_I6_PAGE)
#include <LibWebCommon/HTML/NavigationPopulationRequest.h>
#endif
#ifdef ATRIUM_I6_PAGE
#include <LibWeb/DOM/Document.h>
#include <LibWeb/HTML/LocalNavigable.h>
#endif

struct NavigatorScene;
#ifdef ATRIUM_I6_PAGE
extern "C" bool navigator_live_propose(NavigatorScene*, u8 const*, size_t);
#endif
#ifdef ATRIUM_MAIL_PAGE
extern "C" bool navigator_vpn_script_submit(u8 const*, size_t);
extern "C" bool navigator_vpn_script_post(u8 const*, size_t, u8 const*, size_t);
#endif

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
#ifdef ATRIUM_MAIL_PAGE
        if (target == Web::NavigationTarget::TopLevel && request.has_value() && request->url == url
            && url.serialize() == "https://sso.crypta.test:447/+CSCOE+/saml/sp/continue"sv
            && request->initiator_origin == url.origin() && request->document_resource.has<Web::HTML::POSTResource>()) {
            auto const& post = request->document_resource.get<Web::HTML::POSTResource>();
            if (post.request_content_type == Web::HTML::POSTResource::RequestContentType::ApplicationXWWWFormUrlencoded
                && post.request_body.has_value() && navigator_vpn_script_submit(post.request_body->data(), post.request_body->size()))
                return;
        }
        // Any other form a VPN page submits to its own origin (a script's,
        // such as an IdP's security-key answer) goes to the window, which
        // sends it only on the person's press (vpn.md "Scripts in the
        // sign-in window"). Never followed here.
        if (target == Web::NavigationTarget::TopLevel && request.has_value() && request->url == url
            && request->initiator_origin == url.origin() && request->document_resource.has<Web::HTML::POSTResource>()) {
            auto const& post = request->document_resource.get<Web::HTML::POSTResource>();
            auto const& name = url.serialize();
            if (post.request_content_type == Web::HTML::POSTResource::RequestContentType::ApplicationXWWWFormUrlencoded) {
                u8 const* body = post.request_body.has_value() ? post.request_body->data() : nullptr;
                size_t length = post.request_body.has_value() ? post.request_body->size() : 0;
                if (navigator_vpn_script_post(name.bytes().data(), name.bytes().size(), body, length))
                    return;
            }
        }
#endif
        if (target != Web::NavigationTarget::TopLevel || pending_navigation.has_value()
            || !request.has_value() || request->url != url || !request->document_resource.has<Empty>()
            || request->history_handling != Web::Bindings::NavigationHistoryBehavior::Replace) {
            // Loud: a refused navigation is otherwise a page that just stops.
            warnln("sign-in page: navigation to {} refused (top-level={} same-origin={} post={})", url.serialize(),
                target == Web::NavigationTarget::TopLevel, request.has_value() && request->initiator_origin == url.origin(),
                request.has_value() && request->document_resource.has<Web::HTML::POSTResource>());
            navigation_refused = true;
            return;
        }
        pending_navigation = url;
    }
#endif
#if defined(ATRIUM_I6_PAGE) && !defined(ATRIUM_I0_SIGN_IN)
    // A live page runs its one document. A top-level GET it starts (a link,
    // a GET form, `location`) is proposed to navigatord, which makes it a
    // trail entry (I6.2); the engine never follows it. Anything else (a
    // POST, a frame's document, a jump within this document) is refused,
    // loudly. The base PageClient's VERIFY_NOT_REACHED would end the engine.
    void request_navigation_start(Web::HTML::LocalNavigable& navigable, Web::NavigationTarget target, URL::URL const& url, Utf16String, Optional<Web::HTML::NavigationStartRequest> request) override
    {
        bool get = target == Web::NavigationTarget::TopLevel && request.has_value() && request->document_resource.has<Empty>();
        auto document = navigable.active_document();
        bool same_document = document && url.equals(document->url(), URL::ExcludeFragment::Yes);
        if (get && !same_document && island_scene) {
            auto address = url.serialize();
            if (navigator_live_propose(island_scene, address.bytes().data(), address.bytes().size()))
                return;
        }
        warnln("live page: navigation to {} refused (top-level={} get={} same-document={})", url.serialize(),
            target == Web::NavigationTarget::TopLevel, get, same_document);
    }
#endif
#ifdef ATRIUM_MAIL_PAGE
    // A sign-in page has no dialogs: each is dismissed at once (an IdP's
    // passkey label prompt keeps its default), never left to wait.
    void page_did_request_alert(Utf16String const&) override { m_page->alert_closed(); }
    void page_did_request_confirm(Utf16String const&) override { m_page->confirm_closed(false); }
    void page_did_request_prompt(Utf16String const&, Utf16String const&) override { m_page->prompt_closed({}); }
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
