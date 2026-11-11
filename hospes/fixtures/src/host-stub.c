/* Link-time stand-in for the runner's host table, only to give imports symbol versions.
 * Never loaded. */
int host_versioned_1(void) { return 1; }
int host_versioned_2(void) { return 2; }
int host_versioned_3(void) { return 3; }
__asm__(".symver host_versioned_1, host_versioned@HOST_1");
__asm__(".symver host_versioned_2, host_versioned@@HOST_2");
__asm__(".symver host_versioned_3, host_versioned@HOST_3");
