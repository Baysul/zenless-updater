/*
 * kshd-shim - let a Windows kernel driver under Wine touch kernel-only addresses
 *
 * Wine gives the address space no kernel half: KUSER_SHARED_DATA exists only at the
 * user mapping 0x7FFE0000, and every address from 0xFFFF800000000000 up is unmapped.
 * A driver that reads the shared user data through its kernel alias, or that walks
 * paging structures, takes a SIGSEGV and dies.
 *
 * This shim installs a SIGSEGV handler (wrapping whatever Wine installs) that:
 *   1. redirects KUSER_SHARED_DATA's kernel alias to the real user page, and
 *   2. serves any other kernel-half address from a private shadow arena, so a read
 *      succeeds and returns zeroed memory instead of faulting.
 *
 * Build: cc -shared -fPIC -O2 -o kshd-shim.so kshd-shim.c -ldl
 * Use:   LD_PRELOAD=/path/kshd-shim.so wine ...
 *
 * SPDX-License-Identifier: MIT
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <ucontext.h>
#include <unistd.h>

#define KSHD_ALIAS_BASE 0xFFFFF78000000000ULL /* KUSER_SHARED_DATA, kernel alias */
#define KSHD_USER_BASE  0x000000007FFE0000ULL /* KUSER_SHARED_DATA, user mapping */

#define KERNEL_BASE     0xFFFF800000000000ULL /* start of the kernel half */
#define ARENA_BASE      0x0000700000000000ULL
#define ARENA_BLOCKS    4096u                 /* 4096 * 64 KiB = 256 MiB */
#define BLOCK_SHIFT     16
#define BLOCK_SIZE      (1u << BLOCK_SHIFT)

static int (*real_sigaction)(int, const struct sigaction *, struct sigaction *);
static struct sigaction wine_act;
static int have_wine_act;

static uint64_t block_key[ARENA_BLOCKS]; /* kernel block number + 1, 0 = free */
static size_t n_blocks;

static const int gprs[16] = { REG_RAX, REG_RBX, REG_RCX, REG_RDX, REG_RSI, REG_RDI,
                              REG_RBP, REG_RSP, REG_R8, REG_R9, REG_R10, REG_R11,
                              REG_R12, REG_R13, REG_R14, REG_R15 };

static int arena_up;

#define WATCH_BASE 0x000000005A5A0000ULL /* fake EPROCESS handed to the driver */
static uint64_t watch_seen[64];

/* Map a kernel block to a 64 KiB slot of the shadow arena (capped contiguous). */
static uintptr_t shadow(uintptr_t kaddr)
{
    uintptr_t b = (kaddr - KERNEL_BASE) >> BLOCK_SHIFT;
    uintptr_t off = (kaddr - KERNEL_BASE) & (BLOCK_SIZE - 1);
    uintptr_t h, i;

    if (!arena_up) {
        void *p = mmap((void *)ARENA_BASE, (size_t)ARENA_BLOCKS << BLOCK_SHIFT,
                       PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED | MAP_NORESERVE, -1, 0);
        if (p == MAP_FAILED) return 0;
        arena_up = 1;
    }

    h = (b * 2654435761u) & (ARENA_BLOCKS - 1);
    for (i = 0; i < 64; i++) {
        uintptr_t s = (h + i) & (ARENA_BLOCKS - 1);
        if (block_key[s] == 0) {
            block_key[s] = b + 1;
            n_blocks++;
            if (n_blocks <= 200)
                dprintf(2, "[kshd-shim] pid=%d shadow block %#lx (kernel %#lx)\n",
                        (int)getpid(), (unsigned long)s, (unsigned long)kaddr);
            return ARENA_BASE + (s << BLOCK_SHIFT) + off;
        }
        if (block_key[s] == b + 1) return ARENA_BASE + (s << BLOCK_SHIFT) + off;
    }
    return 0;
}

static int watched(uintptr_t fault)
{
    static int n;
    uintptr_t off = fault - WATCH_BASE, pg, i;
    if (fault < WATCH_BASE || fault >= WATCH_BASE + 0x1000) return 0;
    pg = (fault >> 3) << 3;
    for (i = 0; i < (uintptr_t)n && i < 64; i++) if (watch_seen[i] == pg) break;
    if (i == (uintptr_t)n && n < 64) {
        watch_seen[n++] = pg;
        dprintf(2, "[kshd-shim] pid=%d EPROCESS read +0x%lx\n", (int)getpid(), (unsigned long)off);
    }
    if (mprotect((void *)(fault & ~0xFFFULL), 0x1000, PROT_READ | PROT_WRITE) == 0) return 1;
    return 0;
}

static int fixup(ucontext_t *uc, uintptr_t fault)
{
    greg_t *g = uc->uc_mcontext.gregs;
    uintptr_t repl;
    int i;

    watched(fault);

    if (fault >= KSHD_ALIAS_BASE && fault < KSHD_ALIAS_BASE + 0x1000)
        repl = KSHD_USER_BASE + (fault - KSHD_ALIAS_BASE);
    else if (fault >= KERNEL_BASE)
        repl = shadow(fault);
    else
        return 0;
    if (!repl) return 0;

    for (i = 0; i < 16; i++) {
        uintptr_t v = (uintptr_t)g[gprs[i]];
        if (v == fault) { g[gprs[i]] = (greg_t)repl; return 1; }
        if (fault >= KSHD_ALIAS_BASE && fault < KSHD_ALIAS_BASE + 0x1000 &&
            v >= KSHD_ALIAS_BASE && v < KSHD_ALIAS_BASE + 0x1000) {
            g[gprs[i]] = (greg_t)repl;
            return 1;
        }
        if (fault >= KERNEL_BASE && v >= KERNEL_BASE && v - fault < 0x1000) {
            g[gprs[i]] = (greg_t)repl;
            return 1;
        }
    }
    return 0;
}

static void shim(int sig, siginfo_t *si, void *uctx)
{
    static unsigned hits;
    if (si && si->si_addr && fixup((ucontext_t *)uctx, (uintptr_t)si->si_addr)) {
        if (hits++ < 300)
            dprintf(2, "[kshd-shim] pid=%d redirected read at %p\n", (int)getpid(), si->si_addr);
        return;
    }
    if (have_wine_act && (wine_act.sa_flags & SA_SIGINFO) && wine_act.sa_sigaction)
        wine_act.sa_sigaction(sig, si, uctx);
}

int sigaction(int signum, const struct sigaction *act, struct sigaction *oldact)
{
    if (!real_sigaction) real_sigaction = dlsym(RTLD_NEXT, "sigaction");
    if (signum == SIGSEGV && act) {
        struct sigaction mine = *act;
        have_wine_act = 1;
        wine_act = *act;
        mine.sa_sigaction = shim;
        mine.sa_flags = (act->sa_flags | SA_SIGINFO) & ~(SA_RESETHAND | SA_NODEFER);
        return real_sigaction(signum, &mine, oldact);
    }
    return real_sigaction(signum, act, oldact);
}

__attribute__((constructor)) static void init(void)
{
    real_sigaction = dlsym(RTLD_NEXT, "sigaction");
}
