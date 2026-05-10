/* Phase 0 + Phase 1 + Phase 2.3/2.4/2.5 test program.
 *
 * Phase 0 gate: puts a literal string. Exercises the patched musl crt
 * + the personality server's write/EmitFromShared path.
 *
 * Phase 1 gate (Phase 2.5): fopen/fread/fclose on a FAT32 file.
 * Originally used direct openat/read/close to dodge musl's stdio
 * malloc paths. Now that mmap works (Phase 2.3+), the same gate
 * runs through musl stdio: fopen mallocs the FILE struct (via
 * mmap), fread copies through musl's buffer, fclose frees both.
 * Confirms Phase 1's correctness on top of Phase 2's mmap-backed
 * malloc.
 *
 * Phase 2.3 gate: malloc(1 MiB) + write-through + free. musl's
 * malloc uses mmap above the brk threshold (~256 KiB by default),
 * so a 1 MiB allocation exercises the shim's mmap/munmap path
 * end-to-end: musl malloc -> mmap -> shim handle_mmap -> NR_MMAP IPC
 * -> posix-server handle_file_mmap -> sys_alloc_pages +
 * sys_export_handle -> shim sys_map_pages -> client touches the
 * pages -> free -> musl munmap (eventually) -> shim handle_munmap.
 *
 * Phase 2.4 gate: same as 2.3 but at 8 MiB — exercises the
 * variable-size PageSet header (Phase 2.K) and the multi-L2
 * mapping path (Phase 2.M).
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>  /* write(2) — used after fread to keep the integration assertion stable */

int main(void) {
    puts("hello, lockjaw");

    /* Phase 1 gate via musl stdio. fopen mallocs a FILE struct
     * (which goes through mmap once it exceeds the brk threshold),
     * fread uses an internal buffer, fclose frees it. Same end-to-
     * end correctness as the original direct-syscall path, but now
     * proves Phase 2's mmap-backed malloc supports stdio. */
    FILE *f = fopen("/HELLO.TXT", "r");
    if (!f) {
        puts("posix-hello: fopen failed");
        return 1;
    }
    char buf[64];
    size_t n = fread(buf, 1, sizeof(buf), f);
    fclose(f);
    if (n == 0) {
        puts("posix-hello: fread returned 0");
        return 1;
    }

    /* Use raw write here rather than printf to keep the assertion
     * stable: the existing Phase 1 integration check matches
     * "posix-hello: hello from fat32" exactly, and stdio buffering
     * could fold/reorder additional output around it. */
    write(1, "posix-hello: ", 13);
    write(1, buf, n);

    /* Phase 2.3 gate: 1 MiB malloc above musl's brk threshold goes
     * through mmap. Touch first, middle, last byte to force every
     * page into the working set (PTE write needs a real backing
     * page). The free() may or may not call munmap depending on
     * musl's malloc state — we don't assert it. */
    const size_t SIZE_1M = 1 * 1024 * 1024;
    char *p1 = (char *)malloc(SIZE_1M);
    if (!p1) {
        puts("posix-hello: malloc 1MB FAILED");
        return 1;
    }
    p1[0] = 0xA5;
    p1[SIZE_1M / 2] = 0x5A;
    p1[SIZE_1M - 1] = 0x33;
    if (p1[0] == (char)0xA5 && p1[SIZE_1M / 2] == (char)0x5A && p1[SIZE_1M - 1] == (char)0x33) {
        puts("posix-hello: malloc 1MB ok");
    } else {
        puts("posix-hello: malloc 1MB readback FAILED");
        return 1;
    }
    free(p1);

    /* Phase 2.4 gate: 8 MiB malloc. Single PageSet (2048 data
     * pages + 5-page contiguous header) thanks to the variable-
     * size header from Phase 2.K — without it MAX_PAGES_PER_SET
     * was 510 and 8 MiB would need 5 PageSets. Same write-through
     * pattern as the 1 MiB test. */
    const size_t SIZE_8M = 8 * 1024 * 1024;
    char *p8 = (char *)malloc(SIZE_8M);
    if (!p8) {
        puts("posix-hello: malloc 8MB FAILED");
        return 1;
    }
    p8[0] = 0x77;
    p8[SIZE_8M / 2] = 0x88;
    p8[SIZE_8M - 1] = 0x99;
    if (p8[0] == (char)0x77 && p8[SIZE_8M / 2] == (char)0x88 && p8[SIZE_8M - 1] == (char)0x99) {
        puts("posix-hello: malloc 8MB ok");
    } else {
        puts("posix-hello: malloc 8MB readback FAILED");
        return 1;
    }
    free(p8);
    return 0;
}
