# The example guests, built with the same function a consumer would use, so
# the two cannot drift apart.

tocat_add_wasm_guest(toupper SOURCES toupper.c)
tocat_add_wasm_guest(lines SOURCES lines.cpp)
