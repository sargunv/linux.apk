# SPDX-License-Identifier: AGPL-3.0-or-later
# The props shim tests/bionic/run.sh preloads for read-only test properties.
add_library(props_shim SHARED props-shim.c)
set_target_properties(props_shim PROPERTIES C_VISIBILITY_PRESET hidden)
target_compile_options(props_shim PRIVATE -Wall -Werror)
