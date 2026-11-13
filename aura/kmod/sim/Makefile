# aura_sim.ko: a simulated radio for aura_wlan (docs/spec/wifi.md §6.2). Test
# only: never in devices.d, never loaded on a shipped system. Its node
# /dev/aura/sim0 is a pipe to a user-space bridge (aura-sim-bridge), which
# plays the air: it gets every request and frame the radio would act on and
# writes back what the radio would receive. The module parses nothing.
KMOD=	aura_sim
SRCS=	aura_sim.c

.include <bsd.kmod.mk>
