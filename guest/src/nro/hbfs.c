// A homebrew-format (NRO) test program for the file system (Switchbrew,
// "Filesystem services" and "HIPC"): fsp-srv as libnx's fsInitialize opens
// it (a CMIF domain, SetCurrentProcess, two CloneCurrentObject sessions),
// OpenSdCardFileSystem as fsdevMountSdmc sends it, then the IFileSystem,
// IFile and IDirectory commands the "sdmc:" device sends, laid out as
// libnx lays them out: paths in pointer (X) buffers, file data in send (A)
// and receive (B) buffers. The card starts empty; the program leaves it
// empty. The output is modelled in scripts/gen-expected.py.
#include "hb.h"

#define SFCI 0x49434653u
#define SFCO 0x4f434653u
#define PATH_SIZE 0x301
#define ENTRY_SIZE 0x310

// ---- system calls ---------------------------------------------------------

#define SVC_HANDLE(name, n)                                                  \
    static u64 name(u64 h) {                                                 \
        register u64 x0 __asm__("x0") = h;                                   \
        __asm__ volatile("svc #" #n : "+r"(x0) : : "memory");                \
        return x0;                                                           \
    }
SVC_HANDLE(close_handle, 0x16)
SVC_HANDLE(send_sync_request, 0x21)

static u64 connect(u64 *h, const char *name) {
    register u64 x0 __asm__("x0");
    register u64 x1 __asm__("x1") = (u64)name;
    __asm__ volatile("svc #0x1f" : "=r"(x0), "+r"(x1) : : "memory");
    *h = x1;
    return x0;
}

// ---- messages ---------------------------------------------------------------

static volatile u32 *msg(void) {
    u64 v;
    __asm__ volatile("mrs %0, tpidrro_el0" : "=r"(v));
    return (volatile u32 *)v;
}

typedef struct {
    u64 addr, size;
} buffer;

static char paths[2][PATH_SIZE];
static u32 moved, n_objects;
static volatile u8 *out;

// A request as libnx's hipcMakeRequest and cmifMakeRequest lay it out:
// command type `type` (4 request, 5 control), an optional process ID,
// the paths as pointer (X) buffers 0 and 1, one optional send (A) and
// receive (B) buffer, then the domain header when `object` is not 0.
static void request(u32 type, int pid, u32 object, u32 cmd, const void *args, u32 size,
                    const char *x0, const char *x1, buffer a, buffer b) {
    volatile u32 *m = msg();
    const char *x[2] = {x0, x1};
    u32 nx = x1 ? 2 : x0 ? 1 : 0, na = a.addr != 0, nb = b.addr != 0;
    u32 bytes = 16 + (object ? 16 : 0) + 16 + size;
    bytes = (bytes + 1) & ~1u;
    m[0] = type | nx << 16 | na << 20 | nb << 24;
    m[1] = (bytes + 3) / 4 | (pid ? 1u << 31 : 0);
    u32 at = 2;
    if (pid) {
        m[at++] = 1;
        m[at++] = 0;
        m[at++] = 0;
    }
    for (u32 i = 0; i < nx; i++) {
        for (u32 j = 0; j < PATH_SIZE; j++) paths[i][j] = 0;
        for (u32 j = 0; x[i][j]; j++) paths[i][j] = x[i][j];
        u64 addr = (u64)paths[i];
        m[at++] = i | (u32)(addr >> 36 & 0x3f) << 6 | (u32)(addr >> 32 & 0xf) << 12 |
                  (u32)PATH_SIZE << 16;
        m[at++] = (u32)addr;
    }
    buffer ab[2] = {a, b};
    for (u32 i = 0; i < 2; i++) {
        if (!ab[i].addr) continue;
        m[at++] = (u32)ab[i].size;
        m[at++] = (u32)ab[i].addr;
        m[at++] = (u32)(ab[i].addr >> 36 & 0x3fffff) << 2 | (u32)(ab[i].size >> 32 & 0xf) << 24 |
                  (u32)(ab[i].addr >> 32 & 0xf) << 28;
    }
    while (at % 4) m[at++] = 0;
    if (object) {
        m[at++] = 1 | (16 + size) << 16; // Request, no objects, data size
        m[at++] = object;
        m[at++] = 0;
        m[at++] = 0;
    }
    m[at] = SFCI;
    m[at + 1] = 0;
    m[at + 2] = cmd;
    m[at + 3] = 0;
    volatile u8 *data = (volatile u8 *)&m[at + 4];
    for (u32 i = 0; i < size; i++) data[i] = ((const u8 *)args)[i];
}

// The reply, as libnx's cmifParseResponse reads it: moved handles, the
// domain header's object count for a domain request, the data.
static u32 reply(int domain) {
    volatile u32 *m = msg();
    u32 at = 2;
    moved = n_objects = 0;
    if (m[1] >> 31) {
        u32 special = m[2], ncopy = special >> 1 & 0xf, nmove = special >> 5 & 0xf;
        at = 3 + ncopy;
        if (nmove) moved = m[at];
        at += nmove;
    }
    while (at % 4) at++;
    if (domain) {
        n_objects = m[at];
        at += 4;
    }
    if (m[at] != SFCO) fail("no SFCO header in the reply");
    out = (volatile u8 *)&m[at + 4];
    return m[at + 2];
}

static const buffer none = {0, 0};

// A request to domain object `object` over session `h`.
static u32 call(u64 h, u32 object, u32 cmd, const void *args, u32 size, const char *x0,
                const char *x1, buffer a, buffer b) {
    request(4, 0, object, cmd, args, size, x0, x1, a, b);
    if (send_sync_request(h) != 0) fail("svcSendSyncRequest failed");
    return reply(1);
}

static u32 control(u64 h, u32 cmd) {
    request(5, 0, 0, cmd, 0, 0, 0, 0, none, none);
    if (send_sync_request(h) != 0) fail("svcSendSyncRequest failed");
    return reply(0);
}

static u64 out_u64(u32 at) {
    u64 v = 0;
    for (int i = 7; i >= 0; i--) v = v << 8 | out[at + i];
    return v;
}

// The first object ID after `size` bytes of output.
static u32 object_after(u32 size) {
    if (n_objects != 1) fail("the reply has no object");
    return (u32)(out_u64(size) & 0xffffffffu);
}

// cmifMakeCloseRequest for a domain object.
static void close_object(u64 h, u32 object) {
    volatile u32 *m = msg();
    m[0] = 4;
    m[1] = 8;
    m[2] = m[3] = 0;
    m[4] = 2; // Close
    m[5] = object;
    m[6] = m[7] = 0;
    if (send_sync_request(h) != 0) fail("closing a domain object failed");
}

// ---- output -----------------------------------------------------------------

static void hex(const char *label, u64 v) {
    puts_(label);
    put_hex(v);
    puts_("\n");
}

static void dec(const char *label, u64 v) {
    puts_(label);
    put_u64(v);
    puts_("\n");
}

static void yes_no(const char *label, int v) {
    puts_(label);
    puts_(v ? "yes\n" : "no\n");
}

// ---- the program ------------------------------------------------------------

static u64 fs;  // a clone, as libnx's session manager hands one out
static u32 sd;  // the IFileSystem object

static u32 path_cmd(u32 cmd, const char *path) {
    return call(fs, sd, cmd, 0, 0, path, 0, none, none);
}

static u32 create_file(const char *path, u64 size) {
    struct {
        u32 option, pad;
        u64 size;
    } in = {0, 0, size};
    return call(fs, sd, 0, &in, sizeof in, path, 0, none, none);
}

static u32 open_file(const char *path, u32 mode, u32 *object) {
    u32 rc = call(fs, sd, 8, &mode, 4, path, 0, none, none);
    *object = rc == 0 ? object_after(0) : 0;
    return rc;
}

static u32 entry_type(const char *path) {
    u32 rc = path_cmd(7, path);
    puts_("GetEntryType ");
    puts_(path);
    puts_(": ");
    if (rc == 0) {
        puts_(out[0] == 0 ? "directory\n" : out[0] == 1 ? "file\n" : "?\n");
    } else {
        put_hex(rc);
        puts_("\n");
    }
    return rc;
}

static struct {
    u32 option, pad;
    i64 offset;
    u64 size;
} io;

static u32 file_write(u32 file, i64 offset, const char *data, u64 size) {
    io.option = 0;
    io.offset = offset;
    io.size = size;
    buffer a = {(u64)data, size};
    return call(fs, file, 1, &io, sizeof io, 0, 0, a, none);
}

static char data[64];

// Read into `data`; returns the result and sets *got.
static u32 file_read(u32 file, i64 offset, u64 size, u64 *got) {
    for (u32 i = 0; i < sizeof data; i++) data[i] = 0;
    io.option = 0;
    io.offset = offset;
    io.size = size;
    buffer b = {(u64)data, size};
    u32 rc = call(fs, file, 0, &io, sizeof io, 0, 0, none, b);
    *got = rc == 0 ? out_u64(0) : 0;
    return rc;
}

static u64 file_size(u32 file) {
    if (call(fs, file, 4, 0, 0, 0, 0, none, none) != 0) fail("GetSize failed");
    return out_u64(0);
}

static u8 entries[2 * ENTRY_SIZE];

// IDirectory Read with room for `room` entries; prints each.
static u64 read_entries(u32 dir, u64 room) {
    buffer b = {(u64)entries, room * ENTRY_SIZE};
    if (call(fs, dir, 0, 0, 0, 0, 0, none, b) != 0) fail("IDirectory Read failed");
    u64 n = out_u64(0);
    for (u64 i = 0; i < n; i++) {
        const u8 *e = entries + i * ENTRY_SIZE;
        u64 size = 0;
        for (int j = 7; j >= 0; j--) size = size << 8 | e[0x308 + j];
        puts_("  ");
        puts_((const char *)e);
        puts_(e[0x304] == 0 ? " directory" : " file");
        puts_(", attributes ");
        put_u64(e[0x301]);
        puts_(", size ");
        put_u64(size);
        puts_("\n");
    }
    return n;
}

static u32 open_dir(const char *path, u64 filter, u32 *object) {
    u32 rc = call(fs, sd, 9, &filter, 8, path, 0, none, none);
    *object = rc == 0 ? object_after(0) : 0;
    return rc;
}

static u64 entry_count(u32 dir) {
    if (call(fs, dir, 1, 0, 0, 0, 0, none, none) != 0) fail("GetEntryCount failed");
    return out_u64(0);
}

static const char saved[] = "kestrel saves this";

void hb_main(const config_entry *config, u64 x1) {
    (void)config;
    (void)x1;
    puts_("homebrew fs test\n");

    u64 sm;
    if (connect(&sm, "sm:") != 0) fail("cannot connect to sm:");
    const u64 zero = 0;
    request(4, 1, 0, 0, &zero, 8, 0, 0, none, none);
    if (send_sync_request(sm) != 0) fail("RegisterClient failed");
    hex("sm: register: ", reply(0));
    const char name[8] = "fsp-srv";
    request(4, 0, 0, 1, name, 8, 0, 0, none, none);
    if (send_sync_request(sm) != 0 || reply(0) != 0 || !moved) fail("no fsp-srv");
    u64 root = moved;

    // fsInitialize.
    hex("ConvertCurrentObjectToDomain: ", control(root, 0));
    dec("its object: ", out_u64(0) & 0xffffffffu);
    request(4, 1, 1, 1, &zero, 8, 0, 0, none, none);
    if (send_sync_request(root) != 0) fail("SetCurrentProcess failed");
    hex("SetCurrentProcess: ", reply(1));
    u64 clones[2];
    for (int i = 0; i < 2; i++) {
        hex("CloneCurrentObject: ", control(root, 2));
        clones[i] = moved;
    }
    yes_no("each moved a session handle: ", clones[0] && clones[1] && clones[0] != clones[1]);
    fs = clones[1];

    // fsdevMountSdmc, over the root session.
    hex("OpenSdCardFileSystem: ", call(root, 1, 18, 0, 0, 0, 0, none, none));
    sd = object_after(0);
    yes_no("it gave a domain object: ", sd > 1);
    entry_type("/");

    // Directories and files, through a clone.
    hex("CreateDirectory /switch: ", path_cmd(2, "/switch"));
    hex("CreateDirectory /switch again: ", path_cmd(2, "/switch"));
    hex("CreateDirectory /switch/kestrel: ", path_cmd(2, "/switch/kestrel"));
    hex("CreateFile save.bin, 4 bytes: ", create_file("/switch/kestrel/save.bin", 4));
    hex("CreateFile save.bin again: ", create_file("/switch/kestrel/save.bin", 0));
    hex("CreateFile in a missing directory: ", create_file("/missing/x", 0));
    entry_type("/switch/kestrel/save.bin");
    entry_type("/switch/nothing");
    entry_type("/switch/../switch");
    entry_type("/a:b");
    entry_type("switch");
    entry_type("//");

    // fopen("...", "w+"): write and append; then "r+": write only.
    const char *save = "/switch/kestrel/save.bin";
    u32 appender, writer, reader;
    hex("OpenFile for writing and appending: ", open_file(save, 6, &appender));
    hex("Write 18 bytes: ", file_write(appender, 0, saved, 18));
    hex("OpenFile for writing: ", open_file(save, 2, &writer));
    hex("Write past the end without append: ", file_write(writer, 16, "abcd", 4));
    hex("Write 1 byte at 0: ", file_write(writer, 0, "K", 1));
    hex("OpenFile for reading: ", open_file(save, 1, &reader));
    dec("GetSize: ", file_size(reader));
    u64 got;
    hex("Read 32 bytes: ", file_read(reader, 0, 32, &got));
    dec("bytes read: ", got);
    puts_("they say: ");
    puts_(data);
    puts_("\n");
    hex("Read at the end: ", file_read(reader, 18, 8, &got));
    dec("bytes read: ", got);
    hex("Read at offset -1: ", file_read(reader, -1, 8, &got));
    i64 seven = 7;
    hex("SetSize 7: ", call(fs, writer, 3, &seven, 8, 0, 0, none, none));
    dec("GetSize: ", file_size(reader));
    file_read(reader, 0, 32, &got);
    puts_("they say: ");
    puts_(data);
    puts_("\n");
    hex("Flush: ", call(fs, writer, 2, 0, 0, 0, 0, none, none));
    hex("DeleteFile while it is open: ", path_cmd(1, save));
    hex("RenameFile while it is open: ",
        call(fs, sd, 5, 0, 0, save, "/switch/kestrel/old.bin", none, none));
    close_object(fs, appender);
    close_object(fs, writer);
    close_object(fs, reader);

    // A listing: entries come in name order.
    hex("CreateFile b.txt, 3 bytes: ", create_file("/switch/kestrel/b.txt", 3));
    hex("CreateDirectory sub: ", path_cmd(2, "/switch/kestrel/sub"));
    u32 dir;
    hex("OpenDirectory, files and directories: ", open_dir("/switch/kestrel", 3, &dir));
    dec("GetEntryCount: ", entry_count(dir));
    dec("Read, room for 2: ", read_entries(dir, 2));
    dec("Read, room for 2: ", read_entries(dir, 2));
    dec("Read, room for 2: ", read_entries(dir, 2));
    close_object(fs, dir);
    hex("OpenDirectory, directories only: ", open_dir("/switch/kestrel", 1, &dir));
    dec("GetEntryCount: ", entry_count(dir));
    close_object(fs, dir);
    hex("OpenDirectory of a file: ", open_dir(save, 3, &dir));

    // Rename, timestamps, delete, through the other clone and the root.
    fs = clones[0];
    hex("RenameFile save.bin to sub/save.bin: ",
        call(fs, sd, 5, 0, 0, save, "/switch/kestrel/sub/save.bin", none, none));
    entry_type(save);
    hex("RenameFile onto an existing file: ",
        call(fs, sd, 5, 0, 0, "/switch/kestrel/b.txt", "/switch/kestrel/sub/save.bin", none,
             none));
    hex("RenameDirectory sub to data: ",
        call(fs, sd, 6, 0, 0, "/switch/kestrel/sub", "/switch/kestrel/data", none, none));
    hex("RenameDirectory of a file: ",
        call(fs, sd, 6, 0, 0, "/switch/kestrel/b.txt", "/switch/kestrel/c", none, none));
    hex("GetFileTimeStampRaw: ", path_cmd(14, "/switch/kestrel/b.txt"));
    u64 stamp = 0;
    for (u32 i = 0; i < 0x20; i++) stamp |= out[i];
    yes_no("no timestamps (all zero): ", stamp == 0);
    hex("Commit: ", call(fs, sd, 10, 0, 0, 0, 0, none, none));
    hex("DeleteFile data/save.bin: ", path_cmd(1, "/switch/kestrel/data/save.bin"));
    hex("DeleteFile again: ", path_cmd(1, "/switch/kestrel/data/save.bin"));
    hex("DeleteDirectory data: ", path_cmd(3, "/switch/kestrel/data"));
    hex("DeleteDirectory of a file: ", path_cmd(3, "/switch/kestrel/b.txt"));
    fs = root;
    hex("CleanDirectoryRecursively /switch/kestrel: ", path_cmd(13, "/switch/kestrel"));
    hex("DeleteDirectoryRecursively /switch: ", path_cmd(4, "/switch"));
    hex("OpenDirectory /: ", open_dir("/", 3, &dir));
    dec("GetEntryCount: ", entry_count(dir));
    close_object(fs, dir);

    // fsExit: the file system object, the clones, the root.
    close_object(fs, sd);
    hex("close the clones: ", close_handle(clones[0]) | close_handle(clones[1]));
    hex("close fsp-srv: ", close_handle(root));
    hex("close sm: ", close_handle(sm));
}
