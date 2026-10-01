/* Owned AP09 fixture filesystem. Real FUSE operations return EIO/ENOSPC on
 * command, without filling a real disk or altering another mount. Not a
 * production dependency, encoder shim or simulated decoder/tracker counter. */
#define FUSE_USE_VERSION 31
#define _GNU_SOURCE
#include <fuse3/fuse.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/syscall.h>
#include <unistd.h>

static char backing[PATH_MAX], control[PATH_MAX];
static int fault(int writing) {
    char value[16] = {0};
    int fd = open(control, O_RDONLY | O_NOFOLLOW);
    if (fd < 0) return 0;
    ssize_t count = read(fd, value, sizeof(value) - 1);
    close(fd);
    if (count < 0) return EIO;
    if (!strncmp(value, "EIO", 3)) return EIO;
    return writing && !strncmp(value, "ENOSPC", 6) ? ENOSPC : 0;
}
static int resolve(const char *path, char target[PATH_MAX], int writing) {
    int error = fault(writing);
    if (error) return -error;
    if (path[0] != '/' || strstr(path, "/../") || strstr(path, "/./") ||
        !strcmp(path, "/..")) return -EINVAL;
    int count = snprintf(target, PATH_MAX, "%s%s", backing, path);
    return count < 0 || count >= PATH_MAX ? -ENAMETOOLONG : 0;
}
#define TARGET(path, writing) char target[PATH_MAX]; int error = resolve(path, target, writing); if (error) return error
static int get_attr(const char *path, struct stat *out, struct fuse_file_info *info) {
    (void)info; TARGET(path, 0);
    return lstat(target, out) ? -errno : 0;
}
static int read_link(const char *path, char *buffer, size_t bytes) {
    TARGET(path, 0); ssize_t count = readlink(target, buffer, bytes - 1);
    if (count < 0) return -errno;
    buffer[count] = '\0'; return 0;
}
static int make_dir(const char *path, mode_t mode) { TARGET(path, 1); return mkdir(target, mode) ? -errno : 0; }
static int remove_file(const char *path) { TARGET(path, 1); return unlink(target) ? -errno : 0; }
static int remove_dir(const char *path) { TARGET(path, 1); return rmdir(target) ? -errno : 0; }
static int move_file(const char *from, const char *to, unsigned flags) {
    TARGET(from, 1); char next[PATH_MAX]; error = resolve(to, next, 1); if (error) return error;
    return syscall(SYS_renameat2, AT_FDCWD, target, AT_FDCWD, next, flags) ? -errno : 0;
}
static int link_file(const char *from, const char *to) {
    TARGET(from, 1); char next[PATH_MAX]; error = resolve(to, next, 1); if (error) return error;
    return link(target, next) ? -errno : 0;
}
static int mode_file(const char *path, mode_t mode, struct fuse_file_info *info) {
    (void)info; TARGET(path, 1); return chmod(target, mode) ? -errno : 0;
}
static int owner_file(const char *path, uid_t uid, gid_t gid, struct fuse_file_info *info) {
    (void)info; TARGET(path, 1); return lchown(target, uid, gid) ? -errno : 0;
}
static int truncate_file(const char *path, off_t bytes, struct fuse_file_info *info) {
    TARGET(path, 1);
    int result = info ? ftruncate((int)info->fh, bytes) : truncate(target, bytes);
    return result ? -errno : 0;
}
static int open_file(const char *path, struct fuse_file_info *info) {
    TARGET(path, (info->flags & O_ACCMODE) != O_RDONLY);
    int fd = open(target, info->flags | O_CLOEXEC);
    if (fd < 0) return -errno;
    info->fh = (uint64_t)fd; info->direct_io = 1; return 0;
}
static int create_file(const char *path, mode_t mode, struct fuse_file_info *info) {
    TARGET(path, 1); int fd = open(target, info->flags | O_CREAT | O_CLOEXEC, mode);
    if (fd < 0) return -errno;
    info->fh = (uint64_t)fd; info->direct_io = 1; return 0;
}
static int read_file(const char *path, char *buffer, size_t bytes, off_t offset, struct fuse_file_info *info) {
    (void)path; int error = fault(0); if (error) return -error;
    ssize_t count = pread((int)info->fh, buffer, bytes, offset);
    return count < 0 ? -errno : (int)count;
}
static int write_file(const char *path, const char *buffer, size_t bytes, off_t offset, struct fuse_file_info *info) {
    (void)path; int error = fault(1); if (error) return -error;
    ssize_t count = pwrite((int)info->fh, buffer, bytes, offset);
    return count < 0 ? -errno : (int)count;
}
static int release_file(const char *path, struct fuse_file_info *info) {
    (void)path; return close((int)info->fh) ? -errno : 0;
}
static int sync_file(const char *path, int data, struct fuse_file_info *info) {
    (void)path; int error = fault(1); if (error) return -error;
    int result = data ? fdatasync((int)info->fh) : fsync((int)info->fh);
    return result ? -errno : 0;
}
static int sync_dir(const char *path, int data, struct fuse_file_info *info) {
    (void)data; (void)info; TARGET(path, 1); int fd = open(target, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (fd < 0) return -errno;
    int result = fsync(fd), error_number = errno; close(fd); return result ? -error_number : 0;
}
static int stat_fs(const char *path, struct statvfs *out) {
    (void)path; int error = fault(0); if (error) return -error;
    if (statvfs(backing, out)) return -errno;
    if (fault(1) == ENOSPC) out->f_bavail = out->f_bfree = 0;
    return 0;
}
static int read_dir(const char *path, void *buffer, fuse_fill_dir_t fill, off_t offset,
                    struct fuse_file_info *info, enum fuse_readdir_flags flags) {
    (void)offset; (void)info; (void)flags; TARGET(path, 0); DIR *dir = opendir(target);
    if (!dir) return -errno;
    struct dirent *entry;
    while ((entry = readdir(dir))) {
        struct stat metadata = { .st_ino = entry->d_ino, .st_mode = entry->d_type << 12 };
        if (fill(buffer, entry->d_name, &metadata, 0, 0)) break;
    }
    closedir(dir); return 0;
}
static int time_file(const char *path, const struct timespec times[2], struct fuse_file_info *info) {
    (void)info; TARGET(path, 1); return utimensat(AT_FDCWD, target, times, AT_SYMLINK_NOFOLLOW) ? -errno : 0;
}
static off_t seek_file(const char *path, off_t offset, int whence, struct fuse_file_info *info) {
    (void)path; int error = fault(0); if (error) return -error;
    off_t result = lseek((int)info->fh, offset, whence); return result < 0 ? -errno : result;
}
static void *initialize(struct fuse_conn_info *connection, struct fuse_config *configuration) {
    (void)connection; configuration->entry_timeout = 0; configuration->attr_timeout = 0;
    configuration->negative_timeout = 0; return NULL;
}
int main(int argc, char **argv) {
    if (argc != 4 || !realpath(argv[1], backing) || argv[2][0] != '/' || strlen(argv[2]) >= PATH_MAX) {
        fprintf(stderr, "usage: primary-io-fault OWNED_BACKING ABSOLUTE_CONTROL OWNED_MOUNT\n"); return 2;
    }
    strcpy(control, argv[2]);
    struct fuse_operations operations = { .init = initialize, .getattr = get_attr, .readlink = read_link,
        .mkdir = make_dir, .unlink = remove_file, .rmdir = remove_dir, .rename = move_file, .link = link_file, .chmod = mode_file,
        .chown = owner_file, .truncate = truncate_file, .open = open_file, .create = create_file, .read = read_file,
        .write = write_file, .release = release_file, .fsync = sync_file, .fsyncdir = sync_dir, .statfs = stat_fs,
        .readdir = read_dir, .utimens = time_file, .lseek = seek_file };
    char *arguments[] = { argv[0], argv[3], "-f", "-s", "-o", "default_permissions,fsname=ap09-owned-primary" };
    return fuse_main(6, arguments, &operations, NULL);
}
