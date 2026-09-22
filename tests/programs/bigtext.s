// A program whose code is longer than a b reaches: a loop at the start,
// 150 MB of nothing, its callee at the end. Prints the count.
.subsections_via_symbols
.text
.globl _main
.p2align 2
_main:
    stp x29, x30, [sp, #-32]!
    stp x19, x20, [sp, #16]
    mov w19, #0
    mov w20, #50000
1:  bl _late
    add w19, w19, #1
    cmp w19, w20
    b.ne 1b
    adrp x0, msg@PAGE
    add x0, x0, msg@PAGEOFF
    sub sp, sp, #16
    str x19, [sp]
    bl _printf
    add sp, sp, #16
    ldp x19, x20, [sp, #16]
    ldp x29, x30, [sp], #32
    mov w0, #0
    ret
.globl _pad0
.p2align 2
_pad0:
    .space 39321600
.globl _pad1
.p2align 2
_pad1:
    .space 39321600
.globl _pad2
.p2align 2
_pad2:
    .space 39321600
.globl _pad3
.p2align 2
_pad3:
    .space 39321600
.globl _late
.p2align 2
_late:
    ret
.cstring
msg: .asciz "count %d\n"
