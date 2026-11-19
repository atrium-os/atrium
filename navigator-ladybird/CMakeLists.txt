cmake_minimum_required(VERSION 3.25)
project(NavigatorLadybird LANGUAGES CXX)

# Engine-only font integration checkpoint. Point GFX_FFI and URL_FFI at the
# respective RustFFI.h files from the same no-default-features component build.
foreach(var IN ITEMS LADYBIRD_SOURCE ENGINE_DEPS COMPONENT_LIB GFX_FFI URL_FFI FONT_DIR)
    if(NOT EXISTS "${${var}}")
        message(FATAL_ERROR "${var} needs an existing path")
    endif()
endforeach()
if(NOT CMAKE_SYSTEM_NAME STREQUAL "FreeBSD" OR NOT CMAKE_SYSTEM_PROCESSOR STREQUAL "aarch64")
    message(FATAL_ERROR "Use the arm64 FreeBSD cross toolchain")
endif()
find_package(Python3 REQUIRED COMPONENTS Interpreter)
set(lib "${LADYBIRD_SOURCE}/Libraries")
set(gen "${CMAKE_CURRENT_BINARY_DIR}/generated")
configure_file("${LADYBIRD_SOURCE}/AK/Debug.h.in" "${gen}/AK/Debug.h" @ONLY)
configure_file("${LADYBIRD_SOURCE}/AK/Backtrace.h.in" "${gen}/AK/Backtrace.h" @ONLY)
configure_file("${URL_FFI}" "${gen}/LibURL/RustFFI.h" COPYONLY)
configure_file("${GFX_FFI}" "${gen}/RustFFI.h" COPYONLY)
foreach(name IN ITEMS Core Gfx Web)
    string(TOUPPER "${name}" macro)
    file(MAKE_DIRECTORY "${gen}/Lib${name}")
    file(WRITE "${gen}/Lib${name}/Export.h" "#pragma once\n#define ${macro}_API\n")
endforeach()
foreach(type IN ITEMS Point Rect Size)
    set(name "${type}Codecs")
    execute_process(COMMAND "${Python3_EXECUTABLE}" "${LADYBIRD_SOURCE}/Meta/Generators/generate_ipc_definitions.py"
        --input "${lib}/LibGfx/${type}.ipc" --output "${gen}/LibGfx/${name}.h"
        --implementation "${gen}/LibGfx/${name}.cpp" --header-name "${name}.h"
        --library LibGfx --export-macro=GFX_API COMMAND_ERROR_IS_FATAL ANY)
endforeach()

add_executable(font-check font-check.cpp PinnedFonts.cpp)
function(add_units directory)
    foreach(unit IN LISTS ARGN)
        target_sources(font-check PRIVATE "${directory}/${unit}.cpp")
    endforeach()
endfunction()
add_units("${LADYBIRD_SOURCE}/AK" Assertions Base64 ByteString ByteStringImpl CircularBuffer
    CountingStream Demangle Error FFIHelpers FlyString Format GenericLexer Hex JsonArray JsonObject
    JsonParser JsonValue MathFunctions MemoryStream NumberFormat OptionParser Random StackInfo
    Stream String StringBase StringBuilder StringConversions StringUtils StringView ThreadID Time
    Utf16FlyString Utf16String Utf16StringBuilder Utf16StringData Utf16View Utf8View kmalloc
    LexicalPath ConditionVariablePOSIX MutexPOSIX RWLockPOSIX)
add_units("${lib}/LibGfx/Font" Typeface Font FaceDescription FontTable FontVariationSettings
    FontDatabase SystemFallbackFonts SystemFontMatcherNone SystemFontMatcher)
add_units("${lib}/LibCore" AnonymousBuffer System MappedFile)
add_units("${lib}/LibWeb/Platform" FontPlugin)
add_units("${lib}/LibIPC" Encoder File Attachment Message AutoCloseFileDescriptor)

target_compile_features(font-check PRIVATE cxx_std_23)
target_compile_definitions(font-check PRIVATE LADYBIRD_EMBEDDED_FONT_PROVIDER)
target_include_directories(font-check PRIVATE "${gen}" "${FONT_DIR}" "${LADYBIRD_SOURCE}"
    "${lib}" "${ENGINE_DEPS}/include" "${ENGINE_DEPS}/include/mimalloc-2.2" "${ENGINE_DEPS}/include/llvm-libc")
target_compile_options(font-check PRIVATE -ffunction-sections -fdata-sections
    -Wno-unqualified-std-cast-call -Wno-user-defined-literals)
target_link_options(font-check PRIVATE LINKER:--gc-sections)
target_link_libraries(font-check PRIVATE "${ENGINE_DEPS}/lib/libharfbuzz.a" "${ENGINE_DEPS}/lib/libsimdutf.a"
    "${ENGINE_DEPS}/lib/libzmij.a" "${ENGINE_DEPS}/lib/mimalloc-2.2/libmimalloc.a" "${COMPONENT_LIB}" pthread m)

# Stage the Rust manifest alongside build products so the upstream checkout
# stays outside Atrium's source tree.
configure_file(Painter.toml.in "${CMAKE_CURRENT_BINARY_DIR}/painter/Cargo.toml" @ONLY)

# Build the offline editor against an engine-only LibWeb installation. Generated
# and source headers are separate in Ladybird's build tree.
if(DEFINED WEB_GENERATED_HEADERS)
    add_library(editor-host OBJECT EditorPage.cpp editor-check.cpp)
    target_compile_features(editor-host PRIVATE cxx_std_23)
    target_compile_options(editor-host PRIVATE -Wno-unqualified-std-cast-call
        "--embed-dir=${CMAKE_CURRENT_SOURCE_DIR}/../test-assets/navigator-i0")
    target_include_directories(editor-host PRIVATE "${WEB_GENERATED_HEADERS}" "${gen}"
        "${LADYBIRD_SOURCE}" "${LADYBIRD_SOURCE}/Services" "${lib}" "${ENGINE_DEPS}/include")
    if(DEFINED ENGINE_LIBRARIES)
        add_executable(editor-check $<TARGET_OBJECTS:editor-host> PinnedFonts.cpp EditorAudio.cpp)
        target_compile_features(editor-check PRIVATE cxx_std_23)
        target_include_directories(editor-check PRIVATE "${WEB_GENERATED_HEADERS}" "${gen}"
            "${FONT_DIR}" "${LADYBIRD_SOURCE}" "${lib}")
        target_link_libraries(editor-check PRIVATE ${ENGINE_LIBRARIES})
    endif()
endif()
