#include "EditorPage.h"
#include <AK/ScopeGuard.h>
#include <LibGC/Heap.h>
#include <LibWeb/DOM/Document.h>
#include <LibWeb/DOM/Element.h>
#include <LibWeb/HTML/LocalTraversableNavigable.h>
#ifdef ATRIUM_I1_COUNTER
#include <LibWeb/HTML/HistoryExecutor.h>
#endif
#include <LibWeb/HTML/Parser/HTMLParser.h>
#include <LibWeb/HTML/Parser/ParserScriptingMode.h>
#include <LibWeb/Selection/Selection.h>
#include <LibWeb/Layout/RenderDocument.h>
#include <LibWeb/HTML/PaintConfig.h>
#include <LibWeb/HTML/Scripting/Environments.h>
#include <LibWeb/HTML/Scripting/TemporaryExecutionContext.h>
#include <LibWeb/Painting/PaintableTypes.h>
#include <LibWebCommon/UIEvents/KeyCode.h>

struct NavigatorScene;
extern "C" NavigatorScene* navigator_scene_create(bool embedded);
extern "C" bool navigator_editor_input(NavigatorScene*, u8 kind, float* x, float* y, float* dy);
extern "C" void navigator_scene_destroy(NavigatorScene*);
extern "C" bool navigator_scene_sample(NavigatorScene*, Web::Layout::RustFFI::DocumentHost*, bool visible, bool require_blink);

namespace Navigator {

GC_DEFINE_ALLOCATOR(EditorPage);

GC::Ref<EditorPage> EditorPage::create(Gfx::Palette palette)
{
    return GC::Heap::the().allocate<EditorPage>(move(palette));
}

EditorPage::EditorPage(Gfx::Palette palette)
    : m_palette(move(palette))
    , m_page(Web::Page::create(*this))
{
}

#ifdef ATRIUM_I1_COUNTER
void EditorPage::page_did_request_unload_check(Web::HTML::CrossProcessId id, GC::Ref<GC::Function<void(Web::HTML::CheckIfUnloadingIsCanceledResult)>> done)
{
    // The counter hosts one local document. LibWeb checks its unload handlers.
    page().history_executor().run_ui_beforeunload_check({ id }, {}, {}, Web::HTML::UnloadPromptShown::No,
        GC::create_function(heap(), GC::captures(done), [](auto& callback, Web::HTML::CheckIfUnloadingIsCanceledResult result, Web::HTML::UnloadPromptShown) {
            callback->function()(result);
        }));
}

void EditorPage::page_did_request_history_operation(Web::HTML::CrossProcessId, Web::HistoryOperationParameters operation)
{
    if (operation.has<Web::CloseTopLevelTraversableHistoryOperationParameters>())
        as<Web::HTML::LocalTraversableNavigable>(*page().local_traversable()).run_ui_traversable_close_unload_task();
}
#endif

ErrorOr<void> EditorPage::load_fixture(StringView html, bool embedded)
{
    auto traversable = Web::HTML::LocalTraversableNavigable::create_a_new_top_level_traversable(*m_page, nullptr);
    m_page->set_top_level_traversable(traversable);
    traversable->set_viewport_size({ 640, 360 });
    auto document = traversable->active_document();
    VERIFY(document);
    // Replace the initial about:blank tree while preserving its browsing context.
    document->remove_all_children();
    auto parser = Web::HTML::HTMLParser::create_from_byte_string(*document, html, Web::HTML::ParserScriptingMode::Normal, "utf-8"sv);
    parser->run();
    auto editor = document->get_element_by_id(u"editor"sv);
    auto selection = document->get_selection();
    if (!editor || editor->child_element_count() != 128 || document->active_element() != editor.ptr()
        || !selection || selection->anchor_offset() != 17)
        return Error::from_string_literal("editor script did not establish the expected document, focus and caret");
    puts("editor_lines=128 caret_offset=17");
    auto record = [&]() -> bool {
        Web::Layout::ForcedReadScope read { *document };
        document->update_layout(Web::DOM::UpdateLayoutReason::DumpDisplayList);
        return document->has_committed_viewport_box() && document->record_display_list(read,
            Web::HTML::PaintConfig {}, Web::Painting::PaintCommandCacheMode::ReadWrite);
    };
    if (!record())
        return Error::from_string_literal("missing baseline recording");
    auto* scene = navigator_scene_create(embedded);
    if (!scene)
        return Error::from_string_literal("Limen did not assign an editor window");
    ScopeGuard release_scene = [&] { navigator_scene_destroy(scene); };
    auto sample = [&](bool visible) { return navigator_scene_sample(scene, document->layout_node_arena().host(), visible, true); };
    if (!sample(true) || !sample(false) || !sample(true))
        return Error::from_string_literal("baseline or blink scene conversion failed");
    auto line = selection->anchor_node();
    if (!line || line->text_content() != u"Line 004 | alpha beta gamma"_utf16)
        return Error::from_string_literal("unexpected initial caret line");
    float x = 0, y = 0, dy = 0;
    if (embedded && !navigator_editor_input(scene, 0, &x, &y, &dy))
        return Error::from_string_literal("missing focused X press");
    m_page->handle_keydown(Web::UIEvents::KeyCode::Key_X, 0, 'x', false, true);
    if (embedded && !navigator_editor_input(scene, 1, &x, &y, &dy))
        return Error::from_string_literal("missing focused X release");
    m_page->handle_keyup(Web::UIEvents::KeyCode::Key_X, 0, 'x', false);
    if (selection->anchor_node() != line || selection->anchor_offset() != 18 || line->text_content() != u"Line 004 | alpha xbeta gamma"_utf16 || !record() || !sample(true))
        return Error::from_string_literal("key did not produce the expected text, selection and recording");
    puts("key=x line=4 caret_offset=18");
    auto viewport = document->get_element_by_id(u"viewport"sv);
    if (!viewport || viewport->scroll_top() != 0)
        return Error::from_string_literal("unexpected initial scroll offset");
    Web::HTML::TemporaryExecutionContext context { Web::HTML::relevant_realm(*document) };
    if (embedded) {
        if (!navigator_editor_input(scene, 2, &x, &y, &dy))
            return Error::from_string_literal("missing wheel step on the assigned embed");
        Web::DevicePixelPoint position { static_cast<int>(x), static_cast<int>(y) };
        auto precision = Web::WheelDeltaPrecision::Precise;
        auto result = m_page->handle_mousewheel(position, position, 0, 0, 0, 0, dy, 0, Web::wheel_tick_120ths_from_pixel_delta(dy, precision), precision);
        if (result != Web::EventResult::Accepted)
            return Error::from_string_literal("wheel event was not accepted by the editor");
        // LibWeb dispatches the cancelable event. The native presenter owns
        // the default scroll action and resolves the hit on the published tree.
        Web::Layout::ForcedReadScope read { *document };
        Web::Layout::RustFFI::FfiRecordedFirstWheelStepRoute route {};
        if (!Web::Layout::RustFFI::render_state_recorded_route_first_wheel_step(document->layout_node_arena().host(),
                document->wheel_event_listener_state_generation(), { x, y }, { 0, dy }, &route)
            || !route.has_scroller || route.scroller_node_id != viewport->unique_id().value())
            return Error::from_string_literal("wheel input missed the editor scrollport");
        viewport->set_scroll_top(viewport->scroll_top() + dy);
    } else {
        viewport->set_scroll_top(24);
    }
    if (viewport->scroll_top() != 24 || !record() || !sample(true))
        return Error::from_string_literal("missing 24-pixel scroll or recording");
    puts("viewport_scroll_top=24");
    return {};
}

void EditorPage::visit_edges(JS::Cell::Visitor& visitor)
{
    Base::visit_edges(visitor);
    visitor.visit(m_page);
}

void EditorPage::report_finished_handling_input_event(Web::PageId page_id, u64 event_id, Web::EventResult result)
{
    VERIFY(page_id == id());
    m_results.set(event_id, result);
}

}
