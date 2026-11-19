#pragma once

#include <sys/capsicum.h>
#include <sys/sysctl.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>
#include <stdio.h>

namespace Navigator {

inline bool enter_fixture_capability_mode()
{
    int jailed = 0;
    size_t len = sizeof(jailed);
    unsigned mode = 0;
    if (sysctlbyname("security.jail.jailed", &jailed, &len, nullptr, 0) || jailed != 1 || getuid() == 0
        || cap_enter() || cap_getmode(&mode) || mode != 1 || open("/etc/passwd", O_RDONLY) >= 0 || errno != ECAPMODE)
        return false;
    printf("jailed=%d uid=%u capmode=%u denied-path=ECAPMODE\n", jailed, getuid(), mode);
    return true;
}

}
