use wasmtime_environ::{VMOffsets, WasmHeapType, WasmType};

use super::ControlStackFrame;
use crate::{
    abi::{ABIOperand, ABIResults, RetArea, ABI},
    codegen::BuiltinFunctions,
    frame::Frame,
    isa::reg::RegClass,
    masm::{MacroAssembler, OperandSize, RegImm, SPOffset, StackSlot},
    reg::Reg,
    regalloc::RegAlloc,
    stack::{Stack, TypedReg, Val},
};

/// The code generation context.
/// The code generation context is made up of three
/// essential data structures:
///
/// * The register allocator, in charge of keeping the inventory of register
///   availability.
/// * The value stack, which keeps track of the state of the values
///   after each operation.
/// * The current function's frame.
///
/// These data structures normally require cooperating with each other
/// to perform most of the operations needed during the code
/// generation process. The code generation context should
/// be generally used as the single entry point to access
/// the compound functionality provided by its elements.
pub(crate) struct CodeGenContext<'a, 'builtins: 'a> {
    /// The register allocator.
    pub regalloc: RegAlloc,
    /// The value stack.
    pub stack: Stack,
    /// The current function's frame.
    pub frame: Frame,
    /// Reachability state.
    pub reachable: bool,
    /// The built-in functions available to the JIT code.
    pub builtins: &'builtins mut BuiltinFunctions,
    /// A reference to the VMOffsets.
    pub vmoffsets: &'a VMOffsets<u8>,
}

impl<'a, 'builtins> CodeGenContext<'a, 'builtins> {
    /// Create a new code generation context.
    pub fn new(
        regalloc: RegAlloc,
        stack: Stack,
        frame: Frame,
        builtins: &'builtins mut BuiltinFunctions,
        vmoffsets: &'a VMOffsets<u8>,
    ) -> Self {
        Self {
            regalloc,
            stack,
            frame,
            reachable: true,
            builtins,
            vmoffsets,
        }
    }

    /// Request a specific register to the register allocator,
    /// spilling if not available.
    pub fn reg<M: MacroAssembler>(&mut self, named: Reg, masm: &mut M) -> Reg {
        self.regalloc.reg(named, |regalloc| {
            Self::spill_impl(&mut self.stack, regalloc, &self.frame, masm)
        })
    }

    /// Allocate a register for the given WebAssembly type.
    pub fn reg_for_type<M: MacroAssembler>(&mut self, ty: WasmType, masm: &mut M) -> Reg {
        use WasmType::*;
        match ty {
            I32 | I64 => self.reg_for_class(RegClass::Int, masm),
            F32 | F64 => self.reg_for_class(RegClass::Float, masm),
            Ref(rt) => match rt.heap_type {
                WasmHeapType::Func => self.reg_for_class(RegClass::Int, masm),
                ht => unimplemented!("Support for WasmHeapType: {ht}"),
            },
            t => unimplemented!("Support for WasmType: {t}"),
        }
    }

    /// Request the register allocator to provide the next available
    /// register of the specified class.
    pub fn reg_for_class<M: MacroAssembler>(&mut self, class: RegClass, masm: &mut M) -> Reg {
        self.regalloc.reg_for_class(class, &mut |regalloc| {
            Self::spill_impl(&mut self.stack, regalloc, &self.frame, masm)
        })
    }

    /// Convenience wrapper around `CodeGenContext::reg_for_class`, to
    /// request the next available general purpose register.
    pub fn any_gpr<M: MacroAssembler>(&mut self, masm: &mut M) -> Reg {
        self.reg_for_class(RegClass::Int, masm)
    }

    /// Executes the provided function, guaranteeing that the specified set of
    /// registers, if any, remain unallocatable throughout the function's
    /// execution.
    pub fn without<'r, T, M, F>(
        &mut self,
        regs: impl IntoIterator<Item = &'r Reg> + Copy,
        masm: &mut M,
        mut f: F,
    ) -> T
    where
        M: MacroAssembler,
        F: FnMut(&mut Self, &mut M) -> T,
    {
        for r in regs {
            self.reg(*r, masm);
        }

        let result = f(self, masm);

        for r in regs {
            self.free_reg(*r);
        }

        result
    }

    /// Free the given register.
    pub fn free_reg(&mut self, reg: impl Into<Reg>) {
        let reg: Reg = reg.into();
        self.regalloc.free(reg);
    }

    /// Loads the stack top value into the next available register, if
    /// it isn't already one; spilling if there are no registers
    /// available.  Optionally the caller may specify a specific
    /// destination register.
    /// When a named register is requested and it's not at the top of the
    /// stack a move from register to register might happen, in which case
    /// the source register will be freed.
    pub fn pop_to_reg<M: MacroAssembler>(&mut self, masm: &mut M, named: Option<Reg>) -> TypedReg {
        let typed_reg = if let Some(dst) = named {
            self.stack.pop_named_reg(dst)
        } else {
            self.stack.pop_reg()
        };

        if let Some(dst) = typed_reg {
            return dst;
        }

        let val = self.stack.pop().expect("a value at stack top");
        let reg = if let Some(r) = named {
            self.reg(r, masm)
        } else {
            self.reg_for_type(val.ty(), masm)
        };

        if val.is_mem() {
            let mem = val.unwrap_mem();
            debug_assert_eq!(mem.slot.offset.as_u32(), masm.sp_offset().as_u32());
            masm.pop(reg, val.ty().into());
        } else {
            self.move_val_to_reg(&val, reg, masm);
            // Free the source value if it is a register.
            if val.is_reg() {
                self.free_reg(val.unwrap_reg());
            }
        }

        TypedReg::new(val.ty(), reg)
    }

    /// Pops the value stack top and stores it at the specified address.
    pub fn pop_to_addr<M: MacroAssembler>(&mut self, masm: &mut M, addr: M::Address) {
        let val = self.stack.pop().expect("a value at stack top");
        let size: OperandSize = val.ty().into();
        match val {
            Val::Reg(tr) => {
                masm.store(tr.reg.into(), addr, size);
                self.free_reg(tr.reg);
            }
            Val::I32(v) => masm.store(RegImm::i32(v), addr, size),
            Val::I64(v) => masm.store(RegImm::i64(v), addr, size),
            Val::F32(v) => masm.store(RegImm::f32(v.bits()), addr, size),
            Val::F64(v) => masm.store(RegImm::f64(v.bits()), addr, size),
            Val::Local(local) => {
                let slot = self
                    .frame
                    .get_local(local.index)
                    .unwrap_or_else(|| panic!("invalid local at index = {}", local.index));
                let scratch = <M::ABI as ABI>::scratch_reg();
                let local_addr = masm.local_address(&slot);
                masm.load(local_addr, scratch, size);
                masm.store(scratch.into(), addr, size);
            }
            Val::Memory(_) => {
                let scratch = <M::ABI as ABI>::scratch_reg();
                masm.pop(scratch, size);
                masm.store(scratch.into(), addr, size);
            }
        }
    }

    /// Move a stack value to the given register.
    pub fn move_val_to_reg<M: MacroAssembler>(&self, src: &Val, dst: Reg, masm: &mut M) {
        let size: OperandSize = src.ty().into();
        match src {
            Val::Reg(tr) => masm.mov(RegImm::reg(tr.reg), dst, size),
            Val::I32(imm) => masm.mov(RegImm::i32(*imm), dst, size),
            Val::I64(imm) => masm.mov(RegImm::i64(*imm), dst, size),
            Val::F32(imm) => masm.mov(RegImm::f32(imm.bits()), dst, size),
            Val::F64(imm) => masm.mov(RegImm::f64(imm.bits()), dst, size),
            Val::Local(local) => {
                let slot = self
                    .frame
                    .get_local(local.index)
                    .unwrap_or_else(|| panic!("invalid local at index = {}", local.index));
                let addr = masm.local_address(&slot);
                masm.load(addr, dst, slot.ty.into());
            }
            Val::Memory(mem) => {
                let addr = masm.address_from_sp(mem.slot.offset);
                masm.load(addr, dst, size);
            }
        }
    }

    /// Prepares arguments for emitting a unary operation.
    ///
    /// The `emit` function returns the `TypedReg` to put on the value stack.
    pub fn unop<F, M>(&mut self, masm: &mut M, size: OperandSize, emit: &mut F)
    where
        F: FnMut(&mut M, Reg, OperandSize) -> TypedReg,
        M: MacroAssembler,
    {
        let typed_reg = self.pop_to_reg(masm, None);
        let dst = emit(masm, typed_reg.reg, size);
        self.stack.push(dst.into());
    }

    /// Prepares arguments for emitting a binary operation.
    ///
    /// The `emit` function returns the `TypedReg` to put on the value stack.
    pub fn binop<F, M>(&mut self, masm: &mut M, size: OperandSize, mut emit: F)
    where
        F: FnMut(&mut M, Reg, Reg, OperandSize) -> TypedReg,
        M: MacroAssembler,
    {
        let src = self.pop_to_reg(masm, None);
        let dst = self.pop_to_reg(masm, None);
        let dst = emit(masm, dst.reg, src.reg.into(), size);
        self.free_reg(src);
        self.stack.push(dst.into());
    }

    /// Prepares arguments for emitting an f32 or f64 comparison operation.
    pub fn float_cmp_op<F, M>(&mut self, masm: &mut M, size: OperandSize, mut emit: F)
    where
        F: FnMut(&mut M, Reg, Reg, Reg, OperandSize),
        M: MacroAssembler,
    {
        let src1 = self.pop_to_reg(masm, None);
        let src2 = self.pop_to_reg(masm, None);
        let dst = self.any_gpr(masm);
        emit(masm, dst, src1.reg, src2.reg, size);
        self.free_reg(src1);
        self.free_reg(src2);

        let dst = match size {
            OperandSize::S32 => TypedReg::i32(dst),
            OperandSize::S64 => TypedReg::i64(dst),
            OperandSize::S128 => unreachable!(),
        };
        self.stack.push(dst.into());
    }

    /// Prepares arguments for emitting an i32 binary operation.
    ///
    /// The `emit` function returns the `TypedReg` to put on the value stack.
    pub fn i32_binop<F, M>(&mut self, masm: &mut M, mut emit: F)
    where
        F: FnMut(&mut M, Reg, RegImm, OperandSize) -> TypedReg,
        M: MacroAssembler,
    {
        let top = self.stack.peek().expect("value at stack top");

        if top.is_i32_const() {
            let val = self
                .stack
                .pop_i32_const()
                .expect("i32 const value at stack top");
            let typed_reg = self.pop_to_reg(masm, None);
            let dst = emit(masm, typed_reg.reg, RegImm::i32(val), OperandSize::S32);
            self.stack.push(dst.into());
        } else {
            self.binop(masm, OperandSize::S32, |masm, dst, src, size| {
                emit(masm, dst, src.into(), size)
            });
        }
    }

    /// Prepares arguments for emitting an i64 binary operation.
    ///
    /// The `emit` function returns the `TypedReg` to put on the value stack.
    pub fn i64_binop<F, M>(&mut self, masm: &mut M, mut emit: F)
    where
        F: FnMut(&mut M, Reg, RegImm, OperandSize) -> TypedReg,
        M: MacroAssembler,
    {
        let top = self.stack.peek().expect("value at stack top");
        if top.is_i64_const() {
            let val = self
                .stack
                .pop_i64_const()
                .expect("i64 const value at stack top");
            let typed_reg = self.pop_to_reg(masm, None);
            let dst = emit(masm, typed_reg.reg, RegImm::i64(val), OperandSize::S64);
            self.stack.push(dst.into());
        } else {
            self.binop(masm, OperandSize::S64, |masm, dst, src, size| {
                emit(masm, dst, src.into(), size)
            });
        };
    }

    /// Prepares arguments for emitting a convert operation.
    pub fn convert_op<F, M>(&mut self, masm: &mut M, dst_ty: WasmType, mut emit: F)
    where
        F: FnMut(&mut M, Reg, Reg, OperandSize),
        M: MacroAssembler,
    {
        let src = self.pop_to_reg(masm, None);
        let dst = self.reg_for_type(dst_ty, masm);
        let dst_size = match dst_ty {
            WasmType::I32 => OperandSize::S32,
            WasmType::I64 => OperandSize::S64,
            WasmType::F32 => OperandSize::S32,
            WasmType::F64 => OperandSize::S64,
            WasmType::V128 => unreachable!(),
            WasmType::Ref(_) => unreachable!(),
        };

        emit(masm, dst, src.into(), dst_size);

        self.free_reg(src);
        self.stack.push(TypedReg::new(dst_ty, dst).into());
    }

    /// Prepares arguments for emitting a convert operation with a temporary
    /// register.
    pub fn convert_op_with_tmp_reg<F, M>(
        &mut self,
        masm: &mut M,
        dst_ty: WasmType,
        tmp_reg_class: RegClass,
        mut emit: F,
    ) where
        F: FnMut(&mut M, Reg, Reg, Reg, OperandSize),
        M: MacroAssembler,
    {
        let tmp_gpr = self.reg_for_class(tmp_reg_class, masm);
        self.convert_op(masm, dst_ty, |masm, dst, src, dst_size| {
            emit(masm, dst, src, tmp_gpr, dst_size);
        });
        self.free_reg(tmp_gpr);
    }

    /// Drops the last `n` elements of the stack, calling the provided
    /// function for each `n` stack value.
    /// The values are dropped in top-to-bottom order.
    pub fn drop_last<F>(&mut self, last: usize, mut f: F)
    where
        F: FnMut(&mut RegAlloc, &Val),
    {
        if last > 0 {
            let len = self.stack.len();
            assert!(last <= len);
            let truncate = self.stack.len() - last;
            let stack_mut = self.stack.inner_mut();

            // Invoke the callback in top-to-bottom order.
            for v in stack_mut[truncate..].into_iter().rev() {
                f(&mut self.regalloc, v)
            }
            stack_mut.truncate(truncate);
        }
    }

    /// Convenience wrapper around [`Self::spill_callback`].
    ///
    /// This function exists for cases in which triggering an unconditional
    /// spill is needed, like before entering control flow.
    pub fn spill<M: MacroAssembler>(&mut self, masm: &mut M) {
        Self::spill_impl(&mut self.stack, &mut self.regalloc, &mut self.frame, masm);
    }

    /// Prepares the compiler to emit an uncoditional jump to the given
    /// destination branch.  This process involves:
    /// * Balancing the machine
    ///   stack pointer and value stack by popping it to match the destination
    ///   branch.
    /// * Updating the reachability state.
    /// * Marking the destination frame as a destination target.
    pub fn unconditional_jump<M, F>(&mut self, dest: &mut ControlStackFrame, masm: &mut M, mut f: F)
    where
        M: MacroAssembler,
        F: FnMut(&mut M, &mut Self, &mut ControlStackFrame),
    {
        let state = dest.stack_state();
        let target_offset = state.target_offset;
        let base_offset = state.base_offset;
        // Invariant: The SP, must be greater or equal to the target
        // SP, given that we haven't popped any results by this point
        // yet. But it may happen in the callback.
        assert!(masm.sp_offset().as_u32() >= base_offset.as_u32());
        f(masm, self, dest);

        // The following snippet, pops the stack pointer to ensure that it
        // is correctly placed according to the expectations of the destination
        // branch.
        //
        // This is done in the context of unconditional jumps, as the machine
        // stack might be left unbalanced at the jump site, due to register
        // spills. Note that in some cases the stack pointer offset might be
        // already less than or equal to the original stack pointer offset
        // registered when entering the destination control stack frame, which
        // effectively means that when reaching the jump site no extra space was
        // allocated similar to what would happen in a fall through in which we
        // assume that the program has allocated and deallocated the right
        // amount of stack space.
        //
        // More generally speaking the current stack pointer will be less than
        // the original stack pointer offset in cases in which the top value in
        // the value stack is a memory entry which needs to be popped into the
        // return location according to the ABI (a register for single value
        // returns and a memory slot for 1+ returns). This could happen in the
        // callback invocation above if the callback invokes
        // `ControlStackFrame::pop_abi_results` (e.g. `br` instruction).
        //
        // After an unconditional jump, the compiler will enter in an
        // unreachable state; instead of immediately truncating the value stack
        // to the expected length of the destination branch, we let the
        // reachability analysis code decide what should happen with the length
        // of the value stack once reachability is actually restored. At that
        // point, the right stack pointer offset will also be restored, which
        // should match the contents of the value stack.
        masm.ensure_sp_for_jump(target_offset);
        dest.set_as_target();
        masm.jmp(*dest.label());
        self.reachable = false;
    }

    /// Push the ABI representation of the results stack.
    pub fn push_abi_results<M, F>(
        &mut self,
        results: &ABIResults,
        masm: &mut M,
        mut calculate_ret_area: F,
    ) where
        M: MacroAssembler,
        F: FnMut(&ABIResults, &mut CodeGenContext, &mut M) -> Option<RetArea>,
    {
        let area = results
            .on_stack()
            .then(|| calculate_ret_area(&results, self, masm).unwrap());

        for operand in results.operands().iter() {
            match operand {
                ABIOperand::Reg { reg, ty, .. } => {
                    assert!(self.regalloc.reg_available(*reg));
                    let typed_reg = TypedReg::new(*ty, self.reg(*reg, masm));
                    self.stack.push(typed_reg.into());
                }
                ABIOperand::Stack { ty, offset, size } => match area.unwrap() {
                    RetArea::SP(sp_offset) => {
                        let slot =
                            StackSlot::new(SPOffset::from_u32(sp_offset.as_u32() - offset), *size);
                        self.stack.push(Val::mem(*ty, slot));
                    }
                    // This function is only expected to be called when dealing
                    // with control flow and when calling functions; as a
                    // callee, only [Self::pop_abi_results] is needed when
                    // finalizing the function compilation.
                    _ => unreachable!(),
                },
            }
        }
    }

    /// Truncates the value stack to the specified target.
    /// This function is intended to only be used when restoring the code
    /// generation's reachability state, when handling an unreachable end or
    /// else.
    pub fn truncate_stack_to(&mut self, target: usize) {
        if self.stack.len() > target {
            self.drop_last(self.stack.len() - target, |regalloc, val| match val {
                Val::Reg(tr) => regalloc.free(tr.reg),
                _ => {}
            });
        }
    }

    /// Spill locals and registers to memory.
    // TODO: optimize the spill range;
    // At any point in the program, the stack might already contain memory
    // entries; we could effectively ignore that range; only focusing on the
    // range that contains spillable values.
    fn spill_impl<M: MacroAssembler>(
        stack: &mut Stack,
        regalloc: &mut RegAlloc,
        frame: &Frame,
        masm: &mut M,
    ) {
        stack.inner_mut().iter_mut().for_each(|v| match v {
            Val::Reg(r) => {
                let slot = masm.push(r.reg, r.ty.into());
                regalloc.free(r.reg);
                *v = Val::mem(r.ty, slot);
            }
            Val::Local(local) => {
                let slot = frame.get_local(local.index).expect("valid local at slot");
                let addr = masm.local_address(&slot);
                let scratch = <M::ABI as ABI>::scratch_for(&slot.ty);
                masm.load(addr, scratch, slot.ty.into());
                let stack_slot = masm.push(scratch, slot.ty.into());
                *v = Val::mem(slot.ty, stack_slot);
            }
            _ => {}
        });
    }
}
