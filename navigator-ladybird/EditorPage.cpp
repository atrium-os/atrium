#include "EditorPage.h"
#include <LibGC/Heap.h>
#include <LibWeb/DOM/Document.h>
#include <LibWeb/DOM/Element.h>
#include <LibWeb/HTML/LocalTraversableNavigable.h>
#include <LibWeb/HTML/Parser/HTMLParser.h>
#include <LibWeb/HTML/Parser/ParserScriptingMode.h>
#include <LibWeb/Selection/Selection.h>

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

ErrorOr<void> EditorPage::load_fixture(StringView html)
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
