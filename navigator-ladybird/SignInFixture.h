#pragma once

#include "EditorPage.h"
#include <LibWebCommon/UIEvents/MouseButton.h>
#include <AK/ScopeGuard.h>
#include <LibURL/Parser.h>
#include <LibWeb/DOM/Document.h>
#include <LibWeb/HTML/HTMLButtonElement.h>
#include <LibWeb/HTML/HTMLInputElement.h>
#include <LibWeb/HTML/LocalTraversableNavigable.h>
#include <LibWeb/HTML/PaintConfig.h>
#include <LibWeb/HTML/Parser/HTMLParser.h>
#include <LibWeb/HTML/Parser/ParserScriptingMode.h>
#include <LibWeb/HTML/Scripting/Environments.h>
#include <LibWeb/HTML/Scripting/TemporaryExecutionContext.h>
#include <LibWeb/Layout/RenderDocument.h>
#include <LibWeb/Painting/PaintableTypes.h>
#include <sys/resource.h>

struct NavigatorScene;
extern "C" NavigatorScene* navigator_scene_create(bool);
extern "C" void navigator_scene_destroy(NavigatorScene*);
extern "C" bool navigator_scene_sample(NavigatorScene*, Web::Layout::RustFFI::DocumentHost*, bool, bool);

namespace Navigator {

// The development provider sets this field from an inline script after a
// cookie-authenticated redirect. Exercise DOM, script, layout and scene building.
inline ErrorOr<void> paint_mail_document(EditorPage& client, NavigatorScene* scene, StringView location, StringView html, bool check_fixture)
{
    auto nav = Web::HTML::LocalTraversableNavigable::create_a_new_top_level_traversable(client.page(), nullptr);
    client.page().set_top_level_traversable(nav);
    nav->set_viewport_size({ 640, 360 });
    auto doc = nav->active_document();
    auto parsed = URL::Parser::basic_parse(location);
    if (!parsed.has_value() || parsed->scheme() != "https"sv) return Error::from_string_literal("invalid document origin");
    auto& url = *parsed;
    doc->set_url(url);
    doc->set_origin(url.origin());
    doc->remove_all_children();
    Web::HTML::HTMLParser::create_from_byte_string(*doc, html, Web::HTML::ParserScriptingMode::Normal, "utf-8"sv)->run();
    auto field = doc->get_element_by_id(u"account"sv);
    auto label = doc->get_element_by_id(u"prompt"sv);
    if (check_fixture && (!field || !is<Web::HTML::HTMLInputElement>(*field) || !label || label->text_content() != u"Enter your account"_utf16))
        return Error::from_string_literal("missing account input or script-generated prompt");

    Web::Layout::ForcedReadScope read { *doc };
    doc->update_layout(Web::DOM::UpdateLayoutReason::DumpDisplayList);
    if (!doc->record_display_list(read, Web::HTML::PaintConfig {}, Web::Painting::PaintCommandCacheMode::ReadWrite)
        || !navigator_scene_sample(scene, doc->layout_node_arena().host(), true, false))
        return Error::from_string_literal("development form could not paint");
    if (check_fixture) puts("development_account_input=1 inline_script=1 native_scene=1");
    return {};
}

#ifdef ATRIUM_MAIL_PAGE
extern "C" int navigator_page_pointer(NavigatorScene*, float*, float*);

inline int drive_page(EditorPage& client, NavigatorScene* scene, Core::EventLoop& loop)
{
    auto doc = client.page().local_traversable()->active_document();
    auto tree = doc->dom_tree_version();
    auto text = doc->character_data_version();
    for (unsigned tick = 0; tick < 15000; ++tick) {
        loop.pump(Core::EventLoop::WaitMode::PollForEvents);
        float x = 0, y = 0;
        auto action = navigator_page_pointer(scene, &x, &y);
        if (action < 0) break;
        Web::DevicePixelPoint point { static_cast<int>(x), static_cast<int>(y) };
        if (action == 1) client.page().handle_mousedown(point, point, Web::UIEvents::MouseButton::Primary, Web::UIEvents::MouseButton::Primary, 0, 1);
        if (action == 2) client.page().handle_mouseup(point, point, Web::UIEvents::MouseButton::Primary, 0, 0);
        bool requested = client.take_frame_request();
        if (!requested && !action && tree == doc->dom_tree_version() && text == doc->character_data_version())
            continue;
        tree = doc->dom_tree_version();
        text = doc->character_data_version();
        Web::Layout::ForcedReadScope read { *doc };
        doc->update_layout(Web::DOM::UpdateLayoutReason::DumpDisplayList);
        if (!doc->record_display_list(read, Web::HTML::PaintConfig {}, Web::Painting::PaintCommandCacheMode::ReadWrite)
            || !navigator_scene_sample(scene, doc->layout_node_arena().host(), true, false))
            return 6;
    }
    return 0;
}
#endif

inline ErrorOr<void> run_sign_in(Gfx::Palette palette, Function<ErrorOr<StringView>(Web::Page&, URL::URL const&)> fetch)
{
    // The fixture launcher owns navigation. Each accepted replacement gets a fresh
    // PageClient and realm; only the assigned Fresco scene survives across documents.
    auto* scene = navigator_scene_create(true);
    if (!scene)
        return Error::from_string_literal("missing sign-in embed");
    ScopeGuard cleanup = [&] { navigator_scene_destroy(scene); };
    auto url = URL::Parser::basic_parse("https://idp.example/sign-in.html"sv).value();
    for (auto step : { ""sv, "authorize"sv, "password"sv, "callback"sv }) {
        auto host = EditorPage::create(palette);
        auto& client = *host;
        auto html = TRY(fetch(client.page(), url));
        auto nav = Web::HTML::LocalTraversableNavigable::create_a_new_top_level_traversable(client.page(), nullptr);
        client.page().set_top_level_traversable(nav);
        nav->set_viewport_size({ 640, 360 });
        auto doc = nav->active_document();
        doc->set_url(url);
        doc->set_origin(url.origin());
        doc->remove_all_children();
        Web::HTML::HTMLParser::create_from_byte_string(*doc, html, Web::HTML::ParserScriptingMode::Normal, "utf-8"sv)->run();
        auto status = doc->get_element_by_id(u"result"sv);
        if (!status) return Error::from_string_literal("missing fixture status");
        auto draw = [&] {
            Web::Layout::ForcedReadScope read { *doc };
            doc->update_layout(Web::DOM::UpdateLayoutReason::DumpDisplayList);
            return doc->record_display_list(read, Web::HTML::PaintConfig {}, Web::Painting::PaintCommandCacheMode::ReadWrite)
                && navigator_scene_sample(scene, doc->layout_node_arena().host(), true, false);
        };
        if (step == "password"sv) {
            auto input = doc->get_element_by_id(u"password"sv);
            auto button = doc->query_selector(u"button"_utf16).release_value_but_fixme_should_propagate_errors();
            if (!input || !is<Web::HTML::HTMLInputElement>(*input) || !button || !is<Web::HTML::HTMLButtonElement>(*button)
                || status->text_content() != u"Enter the public fixture password"_utf16 || client.navigation_refused || client.pending_navigation.has_value())
                return Error::from_string_literal("password step did not render");
            if (!draw()) return Error::from_string_literal("password paint failed");
            Web::HTML::TemporaryExecutionContext context { Web::HTML::relevant_realm(*doc) };
            auto& field = static_cast<Web::HTML::HTMLInputElement&>(*input);
            auto& submit = static_cast<Web::HTML::HTMLButtonElement&>(*button);
            if (field.set_value(u"wrong-public-value"sv).is_error()) return Error::from_string_literal("cannot set trial value");
            submit.click();
            if (status->text_content() != u"Fixture password refused"_utf16 || client.navigation_refused || client.pending_navigation.has_value())
                return Error::from_string_literal("rejection did not keep the password page");
            if (!draw()) return Error::from_string_literal("refusal paint failed");
            if (field.set_value(u"atrium-i0-fixture"sv).is_error()) return Error::from_string_literal("cannot set fixture value");
            submit.click();
            if (!field.value().is_empty()) return Error::from_string_literal("fixture failed to clear the field");
        }
        if (step == "callback"sv) {
            if (client.navigation_refused || client.pending_navigation.has_value() || status->text_content() != u"I0 fixture sign-in complete"_utf16 || !draw())
                return Error::from_string_literal("callback status or paint failed");
            struct rusage usage {};
            if (getrusage(RUSAGE_SELF, &usage)) return Error::from_errno(errno);
            outln("sign_in_complete=1 rejected=1 field_cleared=1 maxrss_kib={}", usage.ru_maxrss);
            return {};
        }
        if (client.navigation_refused || !client.pending_navigation.has_value()) return Error::from_string_literal("fixture script did not request navigation");
        url = client.pending_navigation.release_value();
    }
    return Error::from_string_literal("sign-in callback missing");
}
}
