#pragma once

// A live page's localStorage (navigator-islands.md I6.3, second part).
// LibWeb keeps a page's storage areas behind one StorageAreaMirrorClient per
// process; without one every area stays detached and every setItem throws
// QuotaExceededError. For a live page this client is navigatord: the local
// area is the page's site store, loaded once over the island channel and
// written through, item by item; the session area lives only in this engine,
// which is used once.

#ifdef ATRIUM_I6_PAGE

#    include <AK/HashMap.h>
#    include <AK/Utf16String.h>
#    include <AK/Vector.h>
#    include <LibWeb/StorageAPI/StorageAreaMirrorClient.h>

extern "C" bool navigator_live_storage_load(void* ctx, void (*on_item)(void*, u8 const*, size_t, u8 const*, size_t));
extern "C" bool navigator_live_storage_set(u8 const*, size_t, u8 const*, size_t);
extern "C" bool navigator_live_storage_remove(u8 const*, size_t);
extern "C" bool navigator_live_storage_clear();

namespace Navigator {

class LiveStorage final : public Web::StorageAPI::StorageAreaMirrorClient {
public:
    static constexpr u64 local_area = 1;
    static constexpr u64 session_area = 2;

    Web::StorageAPI::StorageAreaConnection connect_to_storage_area(Web::PageId, Web::StorageAPI::StorageEndpointType type, Web::HTML::EnvironmentId const&) override
    {
        using Type = Web::StorageAPI::StorageEndpointType;
        Web::StorageAPI::StorageAreaConnection connection;
        if (type == Type::SessionStorage) {
            connection.id = Web::StorageAPI::StorageAreaId { session_area };
            return connection;
        }
        if (type != Type::LocalStorage)
            return connection;
        if (!m_loaded) {
            m_items.clear();
            if (!navigator_live_storage_load(this, on_item)) {
                // Loud: a detached area is a page whose every setItem fails.
                warnln("live page: navigatord gave no local storage; localStorage stays detached");
                m_items.clear();
                return connection;
            }
            m_loaded = true;
        }
        connection.id = Web::StorageAPI::StorageAreaId { local_area };
        for (auto const& item : m_items) {
            connection.keys.append(item.key);
            connection.values.append(item.value);
            auto k = item.key.to_well_formed_utf8();
            auto v = item.value.to_well_formed_utf8();
            connection.usage += k.bytes().size() + v.bytes().size();
        }
        return connection;
    }

    void disconnect_from_storage_area(Web::StorageAPI::StorageAreaId) override { }

    void set_storage_item(Web::StorageAPI::StorageAreaId id, Utf16String const& key, Utf16String const& value, String const&) override
    {
        if (id.value() != local_area) {
            Web::StorageAPI::storage_area_did_acknowledge_write(id);
            return;
        }
        auto k = key.to_well_formed_utf8();
        auto v = value.to_well_formed_utf8();
        if (navigator_live_storage_set(k.bytes().data(), k.bytes().size(), v.bytes().data(), v.bytes().size())) {
            m_items.set(key, value);
            Web::StorageAPI::storage_area_did_acknowledge_write(id);
            return;
        }
        // navigatord refused it (its quota or bounds): the area goes back to
        // what navigatord keeps, as LibWeb's owner refuses a write.
        warnln("live page: navigatord refused a localStorage item ({} bytes)", k.bytes().size() + v.bytes().size());
        auto [keys, values] = contents();
        Web::StorageAPI::storage_area_did_refuse_write(id, move(keys), move(values));
    }

    void remove_storage_item(Web::StorageAPI::StorageAreaId id, Utf16String const& key, String const&) override
    {
        if (id.value() != local_area) {
            Web::StorageAPI::storage_area_did_acknowledge_write(id);
            return;
        }
        auto k = key.to_well_formed_utf8();
        bool removed = navigator_live_storage_remove(k.bytes().data(), k.bytes().size());
        if (removed)
            m_items.remove(key);
        acknowledge_or_reset(id, removed);
    }

    void clear_storage_area(Web::StorageAPI::StorageAreaId id, String const&) override
    {
        if (id.value() != local_area) {
            Web::StorageAPI::storage_area_did_acknowledge_write(id);
            return;
        }
        bool cleared = navigator_live_storage_clear();
        if (cleared)
            m_items.clear();
        acknowledge_or_reset(id, cleared);
    }

private:
    static void on_item(void* ctx, u8 const* key, size_t key_length, u8 const* value, size_t value_length)
    {
        auto& self = *static_cast<LiveStorage*>(ctx);
        self.m_items.set(Utf16String::from_utf8(StringView { key, key_length }), Utf16String::from_utf8(StringView { value, value_length }));
    }

    struct Contents {
        Vector<Utf16String> keys;
        Vector<Utf16String> values;
    };
    Contents contents() const
    {
        Contents c;
        for (auto const& item : m_items) {
            c.keys.append(item.key);
            c.values.append(item.value);
        }
        return c;
    }

    // Only a set can be refused in LibWeb's protocol: a removal or clear
    // navigatord did not take is acknowledged, then the area is reset to
    // what navigatord keeps, loudly.
    void acknowledge_or_reset(Web::StorageAPI::StorageAreaId id, bool done)
    {
        Web::StorageAPI::storage_area_did_acknowledge_write(id);
        if (done)
            return;
        warnln("live page: navigatord did not take a localStorage removal; the area is reset to its store");
        auto [keys, values] = contents();
        Web::StorageAPI::storage_area_did_reset(id, move(keys), move(values));
    }

    HashMap<Utf16String, Utf16String> m_items;
    bool m_loaded { false };
};

}

#endif
