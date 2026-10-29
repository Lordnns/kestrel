// A tiny register-based bytecode VM, run with two dispatch styles:
// a switch (compiled to a jump table + indirect branch) and a table of
// handler function pointers (indirect calls). Both must agree.
#include "rt.h"

enum { LI, ADD, SUB, MUL, DIVU, MODU, JMP, JLT, JEQ, JNZ, OUT, HALT, NOPS };

typedef struct {
    u8 op, a, b, c;
    i64 imm;
} insn;

typedef struct {
    u64 r[16];
    u64 pc;
    u64 out;
    int halted;
} vm;

static u64 run_switch(const insn *prog) {
    vm m = {0};
    for (;;) {
        const insn *i = &prog[m.pc++];
        switch (i->op) {
        case LI: m.r[i->a] = (u64)i->imm; break;
        case ADD: m.r[i->a] = m.r[i->b] + m.r[i->c]; break;
        case SUB: m.r[i->a] = m.r[i->b] - m.r[i->c]; break;
        case MUL: m.r[i->a] = m.r[i->b] * m.r[i->c]; break;
        case DIVU: m.r[i->a] = m.r[i->b] / m.r[i->c]; break;
        case MODU: m.r[i->a] = m.r[i->b] % m.r[i->c]; break;
        case JMP: m.pc = (u64)i->imm; break;
        case JLT: if (m.r[i->a] < m.r[i->b]) m.pc = (u64)i->imm; break;
        case JEQ: if (m.r[i->a] == m.r[i->b]) m.pc = (u64)i->imm; break;
        case JNZ: if (m.r[i->a]) m.pc = (u64)i->imm; break;
        case OUT: m.out = m.r[i->a]; break;
        case HALT: return m.out;
        default: return ~0ull;
        }
    }
}

typedef void (*handler)(vm *, const insn *);
static void h_li(vm *m, const insn *i) { m->r[i->a] = (u64)i->imm; }
static void h_add(vm *m, const insn *i) { m->r[i->a] = m->r[i->b] + m->r[i->c]; }
static void h_sub(vm *m, const insn *i) { m->r[i->a] = m->r[i->b] - m->r[i->c]; }
static void h_mul(vm *m, const insn *i) { m->r[i->a] = m->r[i->b] * m->r[i->c]; }
static void h_divu(vm *m, const insn *i) { m->r[i->a] = m->r[i->b] / m->r[i->c]; }
static void h_modu(vm *m, const insn *i) { m->r[i->a] = m->r[i->b] % m->r[i->c]; }
static void h_jmp(vm *m, const insn *i) { m->pc = (u64)i->imm; }
static void h_jlt(vm *m, const insn *i) { if (m->r[i->a] < m->r[i->b]) m->pc = (u64)i->imm; }
static void h_jeq(vm *m, const insn *i) { if (m->r[i->a] == m->r[i->b]) m->pc = (u64)i->imm; }
static void h_jnz(vm *m, const insn *i) { if (m->r[i->a]) m->pc = (u64)i->imm; }
static void h_out(vm *m, const insn *i) { m->out = m->r[i->a]; }
static void h_halt(vm *m, const insn *i) { (void)i; m->halted = 1; }

static const handler handlers[NOPS] = {h_li, h_add, h_sub, h_mul, h_divu, h_modu,
                                       h_jmp, h_jlt, h_jeq, h_jnz, h_out, h_halt};

static u64 run_table(const insn *prog) {
    vm m = {0};
    while (!m.halted) {
        const insn *i = &prog[m.pc++];
        handlers[i->op](&m, i);
    }
    return m.out;
}

// sum of k^2 for k = 1..1000
static const insn sum_squares[] = {
    {LI, 0, 0, 0, 0}, {LI, 1, 0, 0, 1}, {LI, 2, 0, 0, 1001}, {LI, 3, 0, 0, 1},
    /* 4 */ {MUL, 4, 1, 1, 0}, {ADD, 0, 0, 4, 0}, {ADD, 1, 1, 3, 0}, {JLT, 1, 2, 0, 4},
    {OUT, 0, 0, 0, 0}, {HALT, 0, 0, 0, 0},
};

// number of Collatz steps from 27 down to 1
static const insn collatz[] = {
    {LI, 0, 0, 0, 27}, {LI, 1, 0, 0, 0}, {LI, 2, 0, 0, 1}, {LI, 3, 0, 0, 2}, {LI, 4, 0, 0, 3},
    /* 5 */ {JEQ, 0, 2, 0, 14},
    /* 6 */ {MODU, 5, 0, 3, 0}, {JNZ, 5, 0, 0, 10},
    /* 8 */ {DIVU, 0, 0, 3, 0}, {JMP, 0, 0, 0, 12},
    /* 10 */ {MUL, 0, 0, 4, 0}, {ADD, 0, 0, 2, 0},
    /* 12 */ {ADD, 1, 1, 2, 0}, {JMP, 0, 0, 0, 5},
    /* 14 */ {OUT, 1, 0, 0, 0}, {HALT, 0, 0, 0, 0},
};

// 20!
static const insn factorial[] = {
    {LI, 0, 0, 0, 1}, {LI, 1, 0, 0, 1}, {LI, 2, 0, 0, 21}, {LI, 3, 0, 0, 1},
    /* 4 */ {MUL, 0, 0, 1, 0}, {ADD, 1, 1, 3, 0}, {JLT, 1, 2, 0, 4},
    {OUT, 0, 0, 0, 0}, {HALT, 0, 0, 0, 0},
};

static void check(const char *name, const insn *prog) {
    u64 a = run_switch(prog);
    u64 b = run_table(prog);
    puts_(name);
    put_u64(a);
    puts_(a == b ? " (both dispatchers agree)\n" : " (DISPATCHERS DISAGREE)\n");
}

int main(void) {
    check("vm sum_squares = ", sum_squares);
    check("vm collatz(27) = ", collatz);
    check("vm 20!         = ", factorial);
    return 0;
}
