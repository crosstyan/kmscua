// libseat 0.6.4's noop backend opens devices without O_NONBLOCK, and libinput
// then blocks forever draining the evdev fd (weston hangs after "Using Pixman
// renderer"). Interpose libseat_open_device and set the flag on the fd.
// Build: gcc -shared -fPIC -O2 -o libseat-nonblock.so libseat-nonblock.c -ldl
// Use:   LD_PRELOAD=/usr/local/lib/kmscua/libseat-nonblock.so weston …
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stddef.h>

struct libseat;
typedef int (*open_fn)(struct libseat *, const char *, int *);

int libseat_open_device(struct libseat *seat, const char *path, int *fd)
{
	// RTLD_NEXT cannot see libseat when it came in through a dlopen'ed
	// backend module, so resolve it from the library itself.
	static open_fn real;
	if (!real) {
		void *h = dlopen("libseat.so.1", RTLD_LAZY | RTLD_NOLOAD);
		if (!h)
			h = dlopen("libseat.so.1", RTLD_LAZY);
		real = h ? (open_fn)dlsym(h, "libseat_open_device") : NULL;
	}
	if (!real)
		return -1;
	int r = real(seat, path, fd);
	if (r >= 0 && fd && *fd >= 0) {
		int fl = fcntl(*fd, F_GETFL);
		if (fl >= 0)
			fcntl(*fd, F_SETFL, fl | O_NONBLOCK);
	}
	return r;
}
