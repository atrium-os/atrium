# atrium.app.mk: build an Atrium C app in a workspace (dev-tools.md §5.4,
# dev-tools-plan.md M4). In the project's Makefile:
#
#	PROG=	hello
#	SRCS=	hello.c
#	ATRIUM_LIBS=	fresco jansson	# the SDK's (fresco, atrium) or any pkg-config name
#	.include <atrium.app.mk>
#
# make		builds ${PROG}
# make run	asks you, in the Terminal, to Run it as an app (its atrium.toml)
# make atrium.toml	writes a first manifest if there is none
# make bundle	an installable bundle of ${PROG} (cargo-atrium bundle, from
#		the Rust toolchain): its libraries, the loader, and any
#		[bundle] extra-libs it loads with dlopen; BUNDLE_FLAGS passes
#		--sign <key.pem>, --out <dir> and the rest
# make clean
#
# The SDK's libraries are in the base (/usr/lib, /usr/include/atrium); the
# rest come from pkg (/usr/local) or a workspace this one uses
# (/workspaces/<name>), through pkg-config when it's installed. Run carries
# the libraries the build links into the app's bundle.

PROG?=		${.CURDIR:T}
SRCS?=		${PROG}.c
OBJS=		${SRCS:R:S/$/.o/g}
CC?=		cc
CFLAGS?=	-O2 -g
CFLAGS+=	-Wall -I/usr/include/atrium
LDFLAGS+=	-Wl,-rpath,/usr/local/lib

.for _l in ${ATRIUM_LIBS}
.  if ${_l} == "fresco"
LDADD+=		-lfresco -lmd
.  elif ${_l} == "atrium"
LDADD+=		-latrium -lthr
.  else
_pc_cflags_${_l}!=	pkg-config --cflags ${_l} 2>/dev/null || echo "-DATRIUM_NO_PKG_CONFIG_${_l:S/-/_/g}"
_pc_libs_${_l}!=	pkg-config --libs ${_l} 2>/dev/null || echo -l${_l}
CFLAGS+=	${_pc_cflags_${_l}}
LDADD+=		${_pc_libs_${_l}}
.  endif
.endfor

all: ${PROG}

${PROG}: ${OBJS}
	${CC} ${LDFLAGS} -o ${.TARGET} ${OBJS} ${LDADD}

.c.o:
	${CC} ${CFLAGS} -c ${.IMPSRC} -o ${.TARGET}

# The folder in ~/work this project is, for Run.
_ATRIUM_PROJECT=	${.CURDIR:S|^${HOME}/work||:S|^/||}

run: ${PROG} atrium.toml
	atrium-run "${_ATRIUM_PROJECT}"

atrium.toml:
	@test -f ${.TARGET} || printf '[app]\nid = "org.example.%s"\nname = "%s"\nversion = "0.1"\nentry = "%s"\n\n[capabilities]\n' ${PROG} ${PROG} ${PROG} > ${.TARGET}
	@echo "wrote ${.TARGET}: add the capabilities ${PROG} needs"

CARGO_ATRIUM?=	cargo-atrium
BUNDLE_FLAGS?=

bundle: ${PROG} atrium.toml
	@command -v ${CARGO_ATRIUM} >/dev/null 2>&1 || { echo "make bundle needs cargo-atrium: add the Rust toolchain to this workspace" >&2; exit 1; }
	${CARGO_ATRIUM} bundle ${BUNDLE_FLAGS}

clean:
	rm -f ${PROG} ${OBJS}
	rm -rf bundle

.PHONY: all run bundle clean
