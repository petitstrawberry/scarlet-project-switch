// QEMU loader supplies PC, not the Linux x0 contract. Simulate that handoff.
.text
.global _start
_start:
    msr daifset, #0xf
    mrs x4, mpidr_el1
    and x4, x4, #0xff
    cbnz x4, parked_secondary
    // Simulate secure firmware assigning GIC interrupts to the NS kernel.
    ldr x4, =0x08000080
    mov w5, #-1
    mov x6, #9
0:
    str w5, [x4], #4
    subs x6, x6, #1
    b.ne 0b
    // BL31 opens the secure GICC priority mask before entering NS. With the
    // reset PMR=0, GICv2 ignores NS PMR writes and blocks every NS interrupt.
    ldr x4, =0x08010004
    mov w5, #0xff
    str w5, [x4]
    dsb sy
    mov x4, #0x501  // SCR_EL3.NS | HCE | RW
    msr scr_el3, x4
    msr cptr_el3, xzr
    mov x4, #0x3c9  // Masked EL2h.
    msr spsr_el3, x4
    adr x4, 2f
    msr elr_el3, x4
    eret
2:
.if ENTER_EL1
    mov x4, #(1 << 31)
    msr hcr_el2, x4
    mov x4, #3
    msr cnthctl_el2, x4
    msr cntvoff_el2, xzr
    msr cptr_el2, xzr
    mov x4, #0x3c5
    msr spsr_el2, x4
    adr x4, 1f
    msr elr_el2, x4
    eret
1:
.endif
    ldr x0, =0x8d000000
    mov x1, xzr
    mov x2, xzr
    mov x3, xzr
    ldr x4, =0x80200000
    br x4
parked_secondary:
    wfe
    b parked_secondary
