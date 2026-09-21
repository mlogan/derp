// A constant table in the text between two functions, as hand-written
// assembly keeps its round constants (blst's SHA-256). Its atom has an
// assembler-local name (`l` prefix), which the linker drops, so the
// function table lists it without a symbol; its words decode as
// instructions, and 0x14292967 is a backward `b`. Prints the table's sum.
#include <stdint.h>
#include <stdio.h>

__asm__(
    ".text\n"
    ".p2align 2\n"
    ".globl _before\n"
    "_before:\n"
    "    mov x0, #7\n"
    "    ret\n"
    ".p2align 5\n"
    "ltable:\n"
    "    .long 0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5\n"
    "    .long 0x14292967, 0x34b0bcb5, 0x27b70a85, 0x2e1b2138\n"
    ".p2align 2\n"
    ".globl _table_word\n"
    "_table_word:\n"
    "    adrp x1, ltable@PAGE\n"
    "    add x1, x1, ltable@PAGEOFF\n"
    "    ldr w0, [x1, x0, lsl #2]\n"
    "    ret\n");

int before(void);
uint32_t table_word(int i);

int main(void) {
    uint64_t sum = 0;
    for (int i = 0; i < 8; i++) sum += table_word(i);
    printf("%d %llu\n", before(), (unsigned long long)sum);
    return 0;
}
