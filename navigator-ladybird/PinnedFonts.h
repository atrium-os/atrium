#pragma once

#include <AK/Error.h>

namespace Navigator {
// Install once, before LibWeb initializes its font plugin.
ErrorOr<void> install_pinned_fonts();
}

// A process-lifetime span into the embedded, address-verified SFNTs.
// The input is a borrowed Gfx::Font kept alive by the recorder's FontHandle.
extern "C" u8 const* navigator_pinned_sfnt(void const* font, size_t* length);
