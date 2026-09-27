cmake_minimum_required(VERSION 3.21)
project(atuin-native
    VERSION 0.1.0
    DESCRIPTION "Atuin shell history as native plugins (zsh + pwsh)"
    LANGUAGES C)

# Always generate compile_commands.json for tooling (LSP, clangd, etc.)
set(CMAKE_EXPORT_COMPILE_COMMANDS ON)

if(WIN32)
    set(BUILD_ZSH_MODULE_DEFAULT OFF)
else()
    set(BUILD_ZSH_MODULE_DEFAULT ON)
endif()

# ── Global options ────────────────────────────────────────────────────────
option(BUILD_ZSH_MODULE   "Build the zsh loadable module (Linux/macOS)"     ${BUILD_ZSH_MODULE_DEFAULT})
option(BUILD_PWSH_MODULE  "Build the PowerShell binary module"              ON)
option(BUILD_TESTS        "Build test executables"                          ON)

# Atuin source: "AUTO" (clone from GitHub) or a local path.
set(ATUIN_SOURCE "AUTO" CACHE STRING
    "Atuin source: 'AUTO' to clone from GitHub, or a local path")
# zsh source: "AUTO" (download zsh 5.9.2) or a local path to configured tree.
set(ZSH_SOURCE "AUTO" CACHE STRING
    "zsh source: 'AUTO' to download zsh 5.9.2, or path to configured source tree")
set(ZSH_VERSION "5.9.2" CACHE STRING "zsh version to download (when ZSH_SOURCE=AUTO)")

include(FetchContent)

# ── Platform detection ────────────────────────────────────────────────────
get_property(_is_multi_config GLOBAL PROPERTY GENERATOR_IS_MULTI_CONFIG)
if(_is_multi_config)
    message(STATUS "Multi-config generator detected — use --config <Release|Debug>")
endif()

# ── Find required tools ───────────────────────────────────────────────────
find_program(CARGO cargo REQUIRED)
find_program(DOTNET dotnet)
find_program(PWSH pwsh)
find_program(PYTHON3 python3)

if(BUILD_PWSH_MODULE AND NOT DOTNET)
    message(WARNING "dotnet not found — PowerShell module will not be built")
    set(BUILD_PWSH_MODULE OFF)
endif()

# ══════════════════════════════════════════════════════════════════════════
# Atuin source
# ══════════════════════════════════════════════════════════════════════════
set(ATUIN_LINK "${CMAKE_SOURCE_DIR}/atuin")
if(ATUIN_SOURCE STREQUAL "AUTO")
    message(STATUS "Fetching atuin from GitHub (in-process branch)...")
    FetchContent_Declare(atuin-src
        GIT_REPOSITORY "https://github.com/ElemenTP/atuin"
        GIT_TAG        in-process
        GIT_SHALLOW    TRUE
    )
    FetchContent_MakeAvailable(atuin-src)
    set(ATUIN_SRC_DIR "${atuin-src_SOURCE_DIR}")
else()
    get_filename_component(ATUIN_SRC_DIR "${ATUIN_SOURCE}" ABSOLUTE)
    if(NOT EXISTS "${ATUIN_SRC_DIR}/Cargo.toml")
        message(FATAL_ERROR "ATUIN_SOURCE=${ATUIN_SOURCE} does not contain Cargo.toml")
    endif()
endif()
message(STATUS "Atuin source: ${ATUIN_SRC_DIR}")

# Create a symlink so rust_src/Cargo.toml path dependency resolves correctly:
#   atuin = { path = "../atuin/crates/atuin", features = ["client", "in-process"] }
# Guard against self-referencing symlinks (ATUIN_SRC_DIR == ATUIN_LINK).
get_filename_component(_atuin_real "${ATUIN_LINK}" REALPATH)
if(NOT _atuin_real STREQUAL "${ATUIN_SRC_DIR}")
    file(REMOVE "${ATUIN_LINK}")
    file(CREATE_LINK "${ATUIN_SRC_DIR}" "${ATUIN_LINK}" SYMBOLIC)
    message(STATUS "Atuin symlink: ${ATUIN_LINK} -> ${ATUIN_SRC_DIR}")
endif()

# ══════════════════════════════════════════════════════════════════════════
# zsh source (headers only)
# ══════════════════════════════════════════════════════════════════════════
if(BUILD_ZSH_MODULE)
    if(ZSH_SOURCE STREQUAL "AUTO")
        message(STATUS "Fetching zsh ${ZSH_VERSION}...")
        FetchContent_Declare(zsh-src
            URL "https://sourceforge.net/projects/zsh/files/zsh/${ZSH_VERSION}/zsh-${ZSH_VERSION}.tar.xz/download"
            URL_HASH SHA256=36fa734374b44783582cec09bcd67822e2f992c779ec1624ab5596df078d2f81
        )
        FetchContent_MakeAvailable(zsh-src)
        set(ZSH_SRC_DIR "${zsh-src_SOURCE_DIR}")

        set(ZSH_CONFIGURE_STAMP "${zsh-src_BINARY_DIR}/.configured")
        if(NOT EXISTS "${ZSH_CONFIGURE_STAMP}")
            message(STATUS "Configuring zsh (generating headers)...")
            execute_process(
                COMMAND ./configure --disable-gdbm --disable-pcre
                WORKING_DIRECTORY "${ZSH_SRC_DIR}"
                RESULT_VARIABLE _zsh_conf_rc
                OUTPUT_QUIET
            )
            if(NOT _zsh_conf_rc EQUAL 0)
                message(FATAL_ERROR "zsh configure failed (run manually in ${ZSH_SRC_DIR})")
            endif()
            execute_process(
                COMMAND ${CMAKE_COMMAND} -E env "top_srcdir=${ZSH_SRC_DIR}"
                    bash "${ZSH_SRC_DIR}/Src/mkmakemod.sh" Src Makemod
                WORKING_DIRECTORY "${ZSH_SRC_DIR}"
                RESULT_VARIABLE _zsh_mkm_rc
                OUTPUT_QUIET
            )
            if(NOT _zsh_mkm_rc EQUAL 0)
                message(FATAL_ERROR "zsh mkmakemod.sh failed")
            endif()
            execute_process(
                COMMAND make -f Makemod zsh.mdh
                WORKING_DIRECTORY "${ZSH_SRC_DIR}/Src"
                RESULT_VARIABLE _zsh_mdh_rc
                OUTPUT_QUIET
            )
            if(NOT _zsh_mdh_rc EQUAL 0)
                message(WARNING "make zsh.mdh had errors (headers may be incomplete)")
            endif()
            file(TOUCH "${ZSH_CONFIGURE_STAMP}")
            message(STATUS "zsh headers generated in ${ZSH_SRC_DIR}/Src")
        endif()
    else()
        get_filename_component(ZSH_SRC_DIR "${ZSH_SOURCE}" ABSOLUTE)
    endif()

    if(NOT EXISTS "${ZSH_SRC_DIR}/Src/zsh.mdh")
        message(WARNING "zsh.mdh not found at ${ZSH_SRC_DIR}/Src/ — "
                        "set ZSH_SOURCE to a configured zsh source tree or use AUTO")
        set(BUILD_ZSH_MODULE OFF)
    else()
        set(ZSH_SRC "${ZSH_SRC_DIR}/Src")
        message(STATUS "zsh headers: ${ZSH_SRC}")
    endif()
endif()

# ══════════════════════════════════════════════════════════════════════════
# Rust FFI crate
# ══════════════════════════════════════════════════════════════════════════
set(RUST_SRC_DIR "${CMAKE_SOURCE_DIR}/rust_src")
set(CARGO_RELEASE_FLAG "$<IF:$<CONFIG:Release>,--release,>")
set(CARGO_PROFILE_DIR "$<IF:$<CONFIG:Release>,release,debug>")

set(FFI_NAME "${CMAKE_SHARED_LIBRARY_PREFIX}atuin_ffi${CMAKE_SHARED_LIBRARY_SUFFIX}")
set(FFI_SHARED "${RUST_SRC_DIR}/target/${CARGO_PROFILE_DIR}/${FFI_NAME}")

# Build atuin-ffi via a real OUTPUT rule. A plain add_custom_target has no
# up-to-date check and would re-run `cargo build` on every build; declaring the
# shared library as an OUTPUT lets Ninja only re-run cargo when it is missing
# or one of the inputs below changed.
set(ATUIN_FFI_DEPENDS
    "${RUST_SRC_DIR}/src"
    "${RUST_SRC_DIR}/Cargo.toml"
    "${RUST_SRC_DIR}/Cargo.lock"
)
# Track the Atuin sources consumed through the path dependency. Cargo performs
# the real change detection, but these inputs tell Ninja when it needs to ask
# cargo again after an upstream checkout changes. GLOB_RECURSE + CONFIGURE_DEPENDS
# keeps both content edits and newly added/removed source files covered.
if(EXISTS "${ATUIN_SRC_DIR}/Cargo.toml")
    list(APPEND ATUIN_FFI_DEPENDS "${ATUIN_SRC_DIR}/Cargo.toml")
endif()
if(EXISTS "${ATUIN_SRC_DIR}/Cargo.lock")
    list(APPEND ATUIN_FFI_DEPENDS "${ATUIN_SRC_DIR}/Cargo.lock")
endif()
file(GLOB_RECURSE ATUIN_SOURCE_FILES CONFIGURE_DEPENDS
    "${ATUIN_SRC_DIR}/crates/*.rs"
    "${ATUIN_SRC_DIR}/crates/*.toml"
    "${ATUIN_SRC_DIR}/crates/*.sql"
)
list(APPEND ATUIN_FFI_DEPENDS ${ATUIN_SOURCE_FILES})

# On macOS, rustc's default LC_ID_DYLIB is the ABSOLUTE cargo target path,
# which gets baked into any consumer (atuin_native.so) as its LC_LOAD_DYLIB,
# leaking the build machine's path into the artifact. Rewrite it to a
# relocatable @rpath reference right after `cargo build`.
if(APPLE)
    set(ATUIN_FFI_FIX
        COMMAND install_name_tool -id
            "@rpath/${FFI_NAME}" "${FFI_SHARED}")
else()
    set(ATUIN_FFI_FIX "")
endif()

add_custom_command(
    OUTPUT "${FFI_SHARED}"
    COMMAND ${CARGO} build ${CARGO_RELEASE_FLAG}
    ${ATUIN_FFI_FIX}
    WORKING_DIRECTORY "${RUST_SRC_DIR}"
    DEPENDS ${ATUIN_FFI_DEPENDS}
    COMMENT "Building atuin-ffi (Rust crate, $<CONFIG>)..."
    USES_TERMINAL
)

add_custom_target(atuin_ffi ALL DEPENDS "${FFI_SHARED}")

# ══════════════════════════════════════════════════════════════════════════
# zsh module
# ══════════════════════════════════════════════════════════════════════════
if(BUILD_ZSH_MODULE)
    set(ZSH_MODULE_NAME "atuin_native")

    add_library(${ZSH_MODULE_NAME} SHARED
        "${CMAKE_SOURCE_DIR}/zsh_src/module.c"
    )

    target_include_directories(${ZSH_MODULE_NAME} PRIVATE
        "${ZSH_SRC}"
        "${ZSH_SRC}/Zle"
        "${ZSH_SRC}/Modules"
        "${ZSH_SRC_DIR}"  # for config.h
        "${CMAKE_SOURCE_DIR}/zsh_src"
    )

    target_compile_definitions(${ZSH_MODULE_NAME} PRIVATE
        IMPORTING_MODULE_zshQsmain=1
    )

    target_compile_options(${ZSH_MODULE_NAME} PRIVATE
        -fPIC -O3 -flto -Wall -Wextra
    )

    add_dependencies(${ZSH_MODULE_NAME} atuin_ffi)
    target_link_libraries(${ZSH_MODULE_NAME} PRIVATE
        "${FFI_SHARED}"
        pthread dl m
    )

    if(APPLE)
        target_link_options(${ZSH_MODULE_NAME} PRIVATE
            -Wl,-undefined,dynamic_lookup
        )
    else()
        target_link_options(${ZSH_MODULE_NAME} PRIVATE
            -Wl,--allow-shlib-undefined -Wl,-z,lazy
        )
    endif()

    # zsh hardcodes its loadable-module suffix to ".so" on EVERY platform
    # (DL_EXT=so in zsh's configure.ac), including macOS. Even though this
    # target is a Mach-O dylib on macOS, it MUST be named atuin_native.so or
    # `zmodload` will refuse to find it.
    set_target_properties(${ZSH_MODULE_NAME} PROPERTIES
        LIBRARY_OUTPUT_DIRECTORY "${CMAKE_SOURCE_DIR}/zsh_src/build"
        PREFIX ""
        SUFFIX ".so"
        BUILD_RPATH "${RUST_SRC_DIR}/target/$<IF:$<CONFIG:Release>,release,debug>"
    )
    if(APPLE)
        # macOS has no $ORIGIN. @loader_path makes the installed module resolve
        # libatuin_ffi.dylib in its own directory (the ffi dylib is installed
        # alongside it under lib/zsh/atuin-native).
        set_target_properties(${ZSH_MODULE_NAME} PROPERTIES
            INSTALL_RPATH "@loader_path"
        )
    else()
        set_target_properties(${ZSH_MODULE_NAME} PROPERTIES
            INSTALL_RPATH "$ORIGIN"
        )
    endif()

    if(APPLE)
        add_custom_command(TARGET ${ZSH_MODULE_NAME} POST_BUILD
            COMMAND ${CMAKE_STRIP} -u "$<TARGET_FILE:${ZSH_MODULE_NAME}>"
            COMMAND_EXPAND_LISTS
        )
    else()
        add_custom_command(TARGET ${ZSH_MODULE_NAME} POST_BUILD
            COMMAND ${CMAKE_STRIP} "$<TARGET_FILE:${ZSH_MODULE_NAME}>"
            COMMAND_EXPAND_LISTS
        )
    endif()

    if(UNIX)
        install(TARGETS ${ZSH_MODULE_NAME}
            LIBRARY DESTINATION "lib/zsh/atuin-native"
        )
        install(FILES "${FFI_SHARED}"
            DESTINATION "lib/zsh/atuin-native"
        )
        install(FILES "${CMAKE_SOURCE_DIR}/zsh_src/atuin-native.plugin.zsh"
            DESTINATION "lib/zsh/atuin-native"
        )
    endif()
endif()

# ══════════════════════════════════════════════════════════════════════════
# PowerShell module
# ══════════════════════════════════════════════════════════════════════════
if(BUILD_PWSH_MODULE)
    set(PWSH_PROJECT "${CMAKE_SOURCE_DIR}/pwsh_src/AtuinNative/AtuinNative.csproj")
    set(PWSH_OUT_DIR "${CMAKE_SOURCE_DIR}/pwsh_src/AtuinNative/bin/$<CONFIG>/net8.0")

    add_custom_target(atuin_pwsh ALL
        COMMAND ${DOTNET} build -c "$<CONFIG>" "${PWSH_PROJECT}"
        COMMAND ${CMAKE_COMMAND} -E copy_if_different
            "${FFI_SHARED}"
            "${PWSH_OUT_DIR}/"
        COMMENT "Building AtuinNative (C# + P/Invoke, $<CONFIG>)..."
        DEPENDS atuin_ffi
        USES_TERMINAL
    )

    install(DIRECTORY "${PWSH_OUT_DIR}/"
        DESTINATION "share/pwsh/modules/atuin-native"
        FILES_MATCHING
        PATTERN "*.dll"
        PATTERN "*.psd1"
        PATTERN "*.psm1"
        PATTERN "*.so"
        PATTERN "*.dylib"
    )
    # The manifest (.psd1) and script module (.psm1) live in pwsh_src/ and are
    # copied into the dotnet output dir by the csproj (CopyToOutputDirectory),
    # so the install rule above picks them up automatically.
endif()

# ══════════════════════════════════════════════════════════════════════════
# Tests
# ══════════════════════════════════════════════════════════════════════════
if(BUILD_TESTS)
    # C FFI smoke/system test (dlopen all atuin_* exports).
    add_executable(ffi_smoke "${CMAKE_SOURCE_DIR}/tests/ffi_smoke.c")
    target_link_libraries(ffi_smoke PRIVATE ${CMAKE_DL_LIBS})
    add_dependencies(ffi_smoke atuin_ffi)

    enable_testing()
    add_test(NAME ffi_smoke
        COMMAND ffi_smoke "${FFI_SHARED}"
        WORKING_DIRECTORY "${CMAKE_BINARY_DIR}"
    )

    add_custom_target(test-rust
        COMMAND ${CARGO} test ${CARGO_RELEASE_FLAG} -- --test-threads=1
        WORKING_DIRECTORY "${RUST_SRC_DIR}"
        USES_TERMINAL
    )

    if(BUILD_ZSH_MODULE AND TARGET ${ZSH_MODULE_NAME})
        set(ZSH_MODULE_OUT_DIR "${CMAKE_SOURCE_DIR}/zsh_src/build/$<CONFIG>")

        add_custom_target(test-zsh
            COMMAND ${CMAKE_COMMAND} -E env
                "MODULE_DIR=${ZSH_MODULE_OUT_DIR}"
                zsh "${CMAKE_SOURCE_DIR}/tests/test_zsh.sh"
            DEPENDS ${ZSH_MODULE_NAME}
            USES_TERMINAL
        )

        add_custom_target(test-fork-zsh
            COMMAND ${CMAKE_COMMAND} -E env
                "MODULE_DIR=${ZSH_MODULE_OUT_DIR}"
                zsh "${CMAKE_SOURCE_DIR}/tests/test_fork_zsh.sh"
            DEPENDS ${ZSH_MODULE_NAME}
            USES_TERMINAL
        )

        add_custom_target(test-unload-zsh
            COMMAND ${CMAKE_COMMAND} -E env
                "MODULE_DIR=${ZSH_MODULE_OUT_DIR}"
                zsh "${CMAKE_SOURCE_DIR}/tests/test_unload_zsh.sh"
            DEPENDS ${ZSH_MODULE_NAME}
            USES_TERMINAL
        )

        add_custom_target(test-plugin-zsh
            COMMAND ${CMAKE_COMMAND} -E env
                "MODULE_DIR=${ZSH_MODULE_OUT_DIR}"
                "REPO_ROOT=${CMAKE_SOURCE_DIR}"
                zsh "${CMAKE_SOURCE_DIR}/tests/test_plugin_zsh.sh"
            DEPENDS ${ZSH_MODULE_NAME}
            USES_TERMINAL
        )

        if(PYTHON3)
            add_custom_target(test-tui-zsh
                COMMAND ${CMAKE_COMMAND} -E env
                    "MODULE_DIR=${ZSH_MODULE_OUT_DIR}"
                    "REPO_ROOT=${CMAKE_SOURCE_DIR}"
                    "${PYTHON3}" "${CMAKE_SOURCE_DIR}/tests/test_tui_zsh.py"
                DEPENDS ${ZSH_MODULE_NAME}
                USES_TERMINAL
            )

            # Same TUI test, but against a real `cmake --install` prefix: this
            # is exactly how users consume the plugin (`source .../atuin-native.plugin.zsh`).
            set(INSTALL_TEST_PREFIX "${CMAKE_BINARY_DIR}/install-test")
            set(_install_test_deps ${ZSH_MODULE_NAME})
            if(BUILD_PWSH_MODULE)
                list(APPEND _install_test_deps atuin_pwsh)
            endif()
            add_custom_target(test-install-zsh
                COMMAND ${CMAKE_COMMAND} --install "${CMAKE_BINARY_DIR}"
                    --config "$<CONFIG>" --prefix "${INSTALL_TEST_PREFIX}"
                COMMAND ${CMAKE_COMMAND} -E env
                    "MODULE_DIR=${INSTALL_TEST_PREFIX}/lib/zsh/atuin-native"
                    "PLUGIN=${INSTALL_TEST_PREFIX}/lib/zsh/atuin-native/atuin-native.plugin.zsh"
                    "REPO_ROOT=${CMAKE_SOURCE_DIR}"
                    "${PYTHON3}" "${CMAKE_SOURCE_DIR}/tests/test_tui_zsh.py"
                DEPENDS ${_install_test_deps}
                USES_TERMINAL
            )
        else()
            message(WARNING "python3 not found — zsh TUI integration test target disabled")
        endif()
    endif()

    if(BUILD_PWSH_MODULE)
        if(PWSH)
            add_custom_target(test-pwsh
                COMMAND ${CMAKE_COMMAND} -E env
                    "DLL_DIR=${PWSH_OUT_DIR}"
                    "${PWSH}" -NoProfile -File "${CMAKE_SOURCE_DIR}/tests/test_pwsh.ps1"
                DEPENDS atuin_pwsh
                USES_TERMINAL
            )

            if(PYTHON3)
                add_custom_target(test-tui-pwsh
                    COMMAND ${CMAKE_COMMAND} -E env
                        "DLL_DIR=${PWSH_OUT_DIR}"
                        "PWSH=${PWSH}"
                        "${PYTHON3}" "${CMAKE_SOURCE_DIR}/tests/test_tui_pwsh.py"
                    DEPENDS atuin_pwsh
                    USES_TERMINAL
                )

                set(INSTALL_TEST_PREFIX "${CMAKE_BINARY_DIR}/install-test")
                set(_install_test_deps atuin_pwsh)
                if(BUILD_ZSH_MODULE)
                    list(APPEND _install_test_deps ${ZSH_MODULE_NAME})
                endif()
                add_custom_target(test-install-pwsh
                    COMMAND ${CMAKE_COMMAND} --install "${CMAKE_BINARY_DIR}"
                        --config "$<CONFIG>" --prefix "${INSTALL_TEST_PREFIX}"
                    COMMAND ${CMAKE_COMMAND} -E env
                        "DLL_DIR=${INSTALL_TEST_PREFIX}/share/pwsh/modules/atuin-native"
                        "PWSH=${PWSH}"
                        "${PYTHON3}" "${CMAKE_SOURCE_DIR}/tests/test_tui_pwsh.py"
                    DEPENDS ${_install_test_deps}
                    USES_TERMINAL
                )
            else()
                message(WARNING "python3 not found — pwsh TUI integration test target disabled")
            endif()
        else()
            message(WARNING "pwsh not found — pwsh integration test target disabled")
        endif()

        set(PWSH_TEST_PROJECT "${CMAKE_SOURCE_DIR}/pwsh_src/AtuinNative.Tests/AtuinNative.Tests.csproj")
        add_custom_target(test-pwsh-unit
            COMMAND ${DOTNET} build -c "$<CONFIG>" "${PWSH_TEST_PROJECT}"
            COMMAND ${CMAKE_COMMAND} -E env
                "DOTNET_ROLL_FORWARD=Major"
                "ATUIN_FFI_PATH=${FFI_SHARED}"
                ${DOTNET} run --no-build -c "$<CONFIG>" --project "${PWSH_TEST_PROJECT}"
            DEPENDS atuin_ffi
            USES_TERMINAL
        )
    endif()

    add_custom_target(check
        COMMAND ${CMAKE_CTEST_COMMAND} --output-on-failure -C "$<CONFIG>"
        DEPENDS ffi_smoke
    )

    set(_check_all_deps check test-rust)
    if(BUILD_ZSH_MODULE AND TARGET ${ZSH_MODULE_NAME})
        list(APPEND _check_all_deps
            test-zsh test-fork-zsh test-unload-zsh test-plugin-zsh)
        if(PYTHON3)
            list(APPEND _check_all_deps test-tui-zsh test-install-zsh)
        endif()
    endif()
    if(BUILD_PWSH_MODULE)
        list(APPEND _check_all_deps test-pwsh-unit)
        if(PWSH)
            list(APPEND _check_all_deps test-pwsh)
            if(PYTHON3)
                list(APPEND _check_all_deps test-tui-pwsh test-install-pwsh)
            endif()
        endif()
    endif()
    add_custom_target(check-all DEPENDS ${_check_all_deps})
endif()

# ── Print configuration summary ────────────────────────────────────────────
message(STATUS "")
message(STATUS "atuin-native ${PROJECT_VERSION} — Build Configuration")
message(STATUS "──────────────────────────────────────────────────")
message(STATUS "Generator:         ${CMAKE_GENERATOR}")
message(STATUS "Atuin source:      ${ATUIN_SRC_DIR}")
if(BUILD_ZSH_MODULE)
    message(STATUS "Zsh source:        ${ZSH_SRC_DIR}")
endif()
message(STATUS "Zsh module:        ${BUILD_ZSH_MODULE}")
message(STATUS "PowerShell module: ${BUILD_PWSH_MODULE}")
message(STATUS "Tests:             ${BUILD_TESTS}")
message(STATUS "Cargo:             ${CARGO}")
message(STATUS "dotnet:            ${DOTNET}")
message(STATUS "pwsh:              ${PWSH}")
message(STATUS "compile_commands:  ${CMAKE_EXPORT_COMPILE_COMMANDS}")
message(STATUS "")
message(STATUS "Build with: cmake --build <dir> --config <Release|Debug>")
message(STATUS "Test with:  cmake --build <dir> --config <Release|Debug> --target check-all")
message(STATUS "")
