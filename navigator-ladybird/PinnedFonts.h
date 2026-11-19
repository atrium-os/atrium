#pragma once

#include <AK/Error.h>

namespace Navigator {
// Install once, before LibWeb initializes its font plugin.
ErrorOr<void> install_pinned_fonts();
}
