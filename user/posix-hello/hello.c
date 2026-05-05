/* Phase 0 + Phase 1 test program.
 *
 * Phase 0 gate: puts a literal string. Exercises the patched musl crt
 * + the personality server's write/EmitFromShared path.
 *
 * Phase 1 gate: openat/read/close on a FAT32 file. Exercises the
 * full stack:
 *   musl openat / read / close (direct syscalls, no malloc)
 *   -> shim openat / read / close
 *   -> personality server FileOpen / FileRead / FileClose handlers
 *   -> FsClient (FS-IPC)
 *   -> fat32-server path resolve + FAT walk + cluster read
 *   -> BlockClient
 *   -> virtio-blk-driver MMIO + IRQ
 *   -> QEMU disk-backed test.img (mformat-built FAT32 with HELLO.TXT).
 *
 * NOTE: fopen/fread/printf are avoided because musl's stdio mallocs
 * the FILE struct, and musl's malloc uses mmap, which Phase 1
 * doesn't implement. Direct syscalls let us hit the gate without
 * pulling in mmap. mmap support is a later phase.
 */
#include <unistd.h>
#include <fcntl.h>
#include <stdio.h>

int main(void) {
    puts("hello, lockjaw");

    int fd = openat(AT_FDCWD, "/HELLO.TXT", O_RDONLY);
    if (fd < 0) {
        puts("posix-hello: openat failed");
        return 1;
    }

    char buf[64];
    ssize_t n = read(fd, buf, sizeof(buf));
    close(fd);
    if (n <= 0) {
        puts("posix-hello: read returned 0 or error");
        return 1;
    }

    /* Emit "posix-hello: <file contents>" without going through stdio
     * malloc paths. The file contents include a trailing newline. */
    write(1, "posix-hello: ", 13);
    write(1, buf, n);
    return 0;
}
