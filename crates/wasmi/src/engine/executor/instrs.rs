pub use self::call::{dispatch_host_func, ResumableHostError};
use self::return_::ReturnOutcome;
use super::{cache::CachedInstance, InstructionPtr, Stack};
use crate::{
    core::{TrapCode, UntypedVal},
    engine::{
        bytecode::{
            AnyConst32,
            BlockFuel,
            Const16,
            DataSegmentIdx,
            ElementSegmentIdx,
            FuncIdx,
            GlobalIdx,
            Instruction,
            Reg,
            SignatureIdx,
            TableIdx,
        },
        code_map::CodeMap,
        executor::stack::{CallFrame, FrameRegisters, ValueStack},
        DedupFuncType,
    },
    memory::DataSegment,
    module::DEFAULT_MEMORY_INDEX,
    store::StoreInner,
    table::ElementSegment,
    Error,
    Func,
    FuncRef,
    Global,
    Memory,
    Store,
    Table,
};

#[cfg(doc)]
use crate::Instance;

mod binary;
mod branch;
mod call;
mod comparison;
mod conversion;
mod copy;
mod global;
mod load;
mod memory;
mod return_;
mod select;
mod store;
mod table;
mod unary;

macro_rules! forward_return {
    ($expr:expr) => {{
        if let ReturnOutcome::Host = $expr {
            return Ok(());
        }
    }};
}

/// Executes compiled function instructions until either
///
/// - returning from the root function
/// - calling a host function
/// - encountering a trap
///
/// # Errors
///
/// If the execution traps.
#[inline(never)]
pub fn execute_instrs<'engine, T>(
    store: &mut Store<T>,
    stack: &'engine mut Stack,
    code_map: &'engine CodeMap,
) -> Result<(), Error> {
    let instance = stack.calls.instance_expect();
    let cache = CachedInstance::new(&mut store.inner, instance);
    Executor::new(stack, code_map, cache).execute(store)
}

/// An execution context for executing a Wasmi function frame.
#[derive(Debug)]
struct Executor<'engine> {
    /// Stores the value stack of live values on the Wasm stack.
    sp: FrameRegisters,
    /// The pointer to the currently executed instruction.
    ip: InstructionPtr,
    /// The cached instance and instance related data.
    cache: CachedInstance,
    /// The value and call stacks.
    stack: &'engine mut Stack,
    /// The static resources of an [`Engine`].
    ///
    /// [`Engine`]: crate::Engine
    code_map: &'engine CodeMap,
}

impl<'engine> Executor<'engine> {
    /// Creates a new [`Executor`] for executing a Wasmi function frame.
    #[inline(always)]
    pub fn new(
        stack: &'engine mut Stack,
        code_map: &'engine CodeMap,
        cache: CachedInstance,
    ) -> Self {
        let frame = stack
            .calls
            .peek()
            .expect("must have call frame on the call stack");
        // Safety: We are using the frame's own base offset as input because it is
        //         guaranteed by the Wasm validation and translation phase to be
        //         valid for all register indices used by the associated function body.
        let sp = unsafe { stack.values.stack_ptr_at(frame.base_offset()) };
        let ip = frame.instr_ptr();
        Self {
            sp,
            ip,
            cache,
            stack,
            code_map,
        }
    }

    /// Executes the function frame until it returns or traps.
    #[inline(always)]
    fn execute<T>(mut self, store: &mut Store<T>) -> Result<(), Error> {
        use Instruction as Instr;
        loop {
            match *self.ip.get() {
                Instr::Trap { trap_code } => self.execute_trap(trap_code)?,
                Instr::ConsumeFuel { block_fuel } => {
                    self.execute_consume_fuel(&mut store.inner, block_fuel)?
                }
                Instr::Return => {
                    forward_return!(self.execute_return(&mut store.inner))
                }
                Instr::ReturnReg { value } => {
                    forward_return!(self.execute_return_reg(&mut store.inner, value))
                }
                Instr::ReturnReg2 { values } => {
                    forward_return!(self.execute_return_reg2(&mut store.inner, values))
                }
                Instr::ReturnReg3 { values } => {
                    forward_return!(self.execute_return_reg3(&mut store.inner, values))
                }
                Instr::ReturnImm32 { value } => {
                    forward_return!(self.execute_return_imm32(&mut store.inner, value))
                }
                Instr::ReturnI64Imm32 { value } => {
                    forward_return!(self.execute_return_i64imm32(&mut store.inner, value))
                }
                Instr::ReturnF64Imm32 { value } => {
                    forward_return!(self.execute_return_f64imm32(&mut store.inner, value))
                }
                Instr::ReturnSpan { values } => {
                    forward_return!(self.execute_return_span(&mut store.inner, values))
                }
                Instr::ReturnMany { values } => {
                    forward_return!(self.execute_return_many(&mut store.inner, values))
                }
                Instr::ReturnNez { condition } => {
                    forward_return!(self.execute_return_nez(&mut store.inner, condition))
                }
                Instr::ReturnNezReg { condition, value } => {
                    forward_return!(self.execute_return_nez_reg(&mut store.inner, condition, value))
                }
                Instr::ReturnNezReg2 { condition, values } => {
                    forward_return!(self.execute_return_nez_reg2(
                        &mut store.inner,
                        condition,
                        values
                    ))
                }
                Instr::ReturnNezImm32 { condition, value } => {
                    forward_return!(self.execute_return_nez_imm32(
                        &mut store.inner,
                        condition,
                        value
                    ))
                }
                Instr::ReturnNezI64Imm32 { condition, value } => {
                    forward_return!(self.execute_return_nez_i64imm32(
                        &mut store.inner,
                        condition,
                        value
                    ))
                }
                Instr::ReturnNezF64Imm32 { condition, value } => {
                    forward_return!(self.execute_return_nez_f64imm32(
                        &mut store.inner,
                        condition,
                        value
                    ))
                }
                Instr::ReturnNezSpan { condition, values } => {
                    forward_return!(self.execute_return_nez_span(
                        &mut store.inner,
                        condition,
                        values
                    ))
                }
                Instr::ReturnNezMany { condition, values } => {
                    forward_return!(self.execute_return_nez_many(
                        &mut store.inner,
                        condition,
                        values
                    ))
                }
                Instr::Branch { offset } => self.execute_branch(offset),
                Instr::BranchTable0 { index, len_targets } => {
                    self.execute_branch_table_0(index, len_targets)
                }
                Instr::BranchTable1 { index, len_targets } => {
                    self.execute_branch_table_1(index, len_targets)
                }
                Instr::BranchTable2 { index, len_targets } => {
                    self.execute_branch_table_2(index, len_targets)
                }
                Instr::BranchTable3 { index, len_targets } => {
                    self.execute_branch_table_3(index, len_targets)
                }
                Instr::BranchTableSpan { index, len_targets } => {
                    self.execute_branch_table_span(index, len_targets)
                }
                Instr::BranchTableMany { index, len_targets } => {
                    self.execute_branch_table_many(index, len_targets)
                }
                Instr::BranchCmpFallback { lhs, rhs, params } => {
                    self.execute_branch_cmp_fallback(lhs, rhs, params)
                }
                Instr::BranchI32And { lhs, rhs, offset } => {
                    self.execute_branch_i32_and(lhs, rhs, offset)
                }
                Instr::BranchI32AndImm { lhs, rhs, offset } => {
                    self.execute_branch_i32_and_imm(lhs, rhs, offset)
                }
                Instr::BranchI32Or { lhs, rhs, offset } => {
                    self.execute_branch_i32_or(lhs, rhs, offset)
                }
                Instr::BranchI32OrImm { lhs, rhs, offset } => {
                    self.execute_branch_i32_or_imm(lhs, rhs, offset)
                }
                Instr::BranchI32Xor { lhs, rhs, offset } => {
                    self.execute_branch_i32_xor(lhs, rhs, offset)
                }
                Instr::BranchI32XorImm { lhs, rhs, offset } => {
                    self.execute_branch_i32_xor_imm(lhs, rhs, offset)
                }
                Instr::BranchI32AndEqz { lhs, rhs, offset } => {
                    self.execute_branch_i32_and_eqz(lhs, rhs, offset)
                }
                Instr::BranchI32AndEqzImm { lhs, rhs, offset } => {
                    self.execute_branch_i32_and_eqz_imm(lhs, rhs, offset)
                }
                Instr::BranchI32OrEqz { lhs, rhs, offset } => {
                    self.execute_branch_i32_or_eqz(lhs, rhs, offset)
                }
                Instr::BranchI32OrEqzImm { lhs, rhs, offset } => {
                    self.execute_branch_i32_or_eqz_imm(lhs, rhs, offset)
                }
                Instr::BranchI32XorEqz { lhs, rhs, offset } => {
                    self.execute_branch_i32_xor_eqz(lhs, rhs, offset)
                }
                Instr::BranchI32XorEqzImm { lhs, rhs, offset } => {
                    self.execute_branch_i32_xor_eqz_imm(lhs, rhs, offset)
                }
                Instr::BranchI32Eq { lhs, rhs, offset } => {
                    self.execute_branch_i32_eq(lhs, rhs, offset)
                }
                Instr::BranchI32EqImm { lhs, rhs, offset } => {
                    self.execute_branch_i32_eq_imm(lhs, rhs, offset)
                }
                Instr::BranchI32Ne { lhs, rhs, offset } => {
                    self.execute_branch_i32_ne(lhs, rhs, offset)
                }
                Instr::BranchI32NeImm { lhs, rhs, offset } => {
                    self.execute_branch_i32_ne_imm(lhs, rhs, offset)
                }
                Instr::BranchI32LtS { lhs, rhs, offset } => {
                    self.execute_branch_i32_lt_s(lhs, rhs, offset)
                }
                Instr::BranchI32LtSImm { lhs, rhs, offset } => {
                    self.execute_branch_i32_lt_s_imm(lhs, rhs, offset)
                }
                Instr::BranchI32LtU { lhs, rhs, offset } => {
                    self.execute_branch_i32_lt_u(lhs, rhs, offset)
                }
                Instr::BranchI32LtUImm { lhs, rhs, offset } => {
                    self.execute_branch_i32_lt_u_imm(lhs, rhs, offset)
                }
                Instr::BranchI32LeS { lhs, rhs, offset } => {
                    self.execute_branch_i32_le_s(lhs, rhs, offset)
                }
                Instr::BranchI32LeSImm { lhs, rhs, offset } => {
                    self.execute_branch_i32_le_s_imm(lhs, rhs, offset)
                }
                Instr::BranchI32LeU { lhs, rhs, offset } => {
                    self.execute_branch_i32_le_u(lhs, rhs, offset)
                }
                Instr::BranchI32LeUImm { lhs, rhs, offset } => {
                    self.execute_branch_i32_le_u_imm(lhs, rhs, offset)
                }
                Instr::BranchI32GtS { lhs, rhs, offset } => {
                    self.execute_branch_i32_gt_s(lhs, rhs, offset)
                }
                Instr::BranchI32GtSImm { lhs, rhs, offset } => {
                    self.execute_branch_i32_gt_s_imm(lhs, rhs, offset)
                }
                Instr::BranchI32GtU { lhs, rhs, offset } => {
                    self.execute_branch_i32_gt_u(lhs, rhs, offset)
                }
                Instr::BranchI32GtUImm { lhs, rhs, offset } => {
                    self.execute_branch_i32_gt_u_imm(lhs, rhs, offset)
                }
                Instr::BranchI32GeS { lhs, rhs, offset } => {
                    self.execute_branch_i32_ge_s(lhs, rhs, offset)
                }
                Instr::BranchI32GeSImm { lhs, rhs, offset } => {
                    self.execute_branch_i32_ge_s_imm(lhs, rhs, offset)
                }
                Instr::BranchI32GeU { lhs, rhs, offset } => {
                    self.execute_branch_i32_ge_u(lhs, rhs, offset)
                }
                Instr::BranchI32GeUImm { lhs, rhs, offset } => {
                    self.execute_branch_i32_ge_u_imm(lhs, rhs, offset)
                }
                Instr::BranchI64Eq { lhs, rhs, offset } => {
                    self.execute_branch_i64_eq(lhs, rhs, offset)
                }
                Instr::BranchI64EqImm { lhs, rhs, offset } => {
                    self.execute_branch_i64_eq_imm(lhs, rhs, offset)
                }
                Instr::BranchI64Ne { lhs, rhs, offset } => {
                    self.execute_branch_i64_ne(lhs, rhs, offset)
                }
                Instr::BranchI64NeImm { lhs, rhs, offset } => {
                    self.execute_branch_i64_ne_imm(lhs, rhs, offset)
                }
                Instr::BranchI64LtS { lhs, rhs, offset } => {
                    self.execute_branch_i64_lt_s(lhs, rhs, offset)
                }
                Instr::BranchI64LtSImm { lhs, rhs, offset } => {
                    self.execute_branch_i64_lt_s_imm(lhs, rhs, offset)
                }
                Instr::BranchI64LtU { lhs, rhs, offset } => {
                    self.execute_branch_i64_lt_u(lhs, rhs, offset)
                }
                Instr::BranchI64LtUImm { lhs, rhs, offset } => {
                    self.execute_branch_i64_lt_u_imm(lhs, rhs, offset)
                }
                Instr::BranchI64LeS { lhs, rhs, offset } => {
                    self.execute_branch_i64_le_s(lhs, rhs, offset)
                }
                Instr::BranchI64LeSImm { lhs, rhs, offset } => {
                    self.execute_branch_i64_le_s_imm(lhs, rhs, offset)
                }
                Instr::BranchI64LeU { lhs, rhs, offset } => {
                    self.execute_branch_i64_le_u(lhs, rhs, offset)
                }
                Instr::BranchI64LeUImm { lhs, rhs, offset } => {
                    self.execute_branch_i64_le_u_imm(lhs, rhs, offset)
                }
                Instr::BranchI64GtS { lhs, rhs, offset } => {
                    self.execute_branch_i64_gt_s(lhs, rhs, offset)
                }
                Instr::BranchI64GtSImm { lhs, rhs, offset } => {
                    self.execute_branch_i64_gt_s_imm(lhs, rhs, offset)
                }
                Instr::BranchI64GtU { lhs, rhs, offset } => {
                    self.execute_branch_i64_gt_u(lhs, rhs, offset)
                }
                Instr::BranchI64GtUImm { lhs, rhs, offset } => {
                    self.execute_branch_i64_gt_u_imm(lhs, rhs, offset)
                }
                Instr::BranchI64GeS { lhs, rhs, offset } => {
                    self.execute_branch_i64_ge_s(lhs, rhs, offset)
                }
                Instr::BranchI64GeSImm { lhs, rhs, offset } => {
                    self.execute_branch_i64_ge_s_imm(lhs, rhs, offset)
                }
                Instr::BranchI64GeU { lhs, rhs, offset } => {
                    self.execute_branch_i64_ge_u(lhs, rhs, offset)
                }
                Instr::BranchI64GeUImm { lhs, rhs, offset } => {
                    self.execute_branch_i64_ge_u_imm(lhs, rhs, offset)
                }
                Instr::BranchF32Eq { lhs, rhs, offset } => {
                    self.execute_branch_f32_eq(lhs, rhs, offset)
                }
                Instr::BranchF32Ne { lhs, rhs, offset } => {
                    self.execute_branch_f32_ne(lhs, rhs, offset)
                }
                Instr::BranchF32Lt { lhs, rhs, offset } => {
                    self.execute_branch_f32_lt(lhs, rhs, offset)
                }
                Instr::BranchF32Le { lhs, rhs, offset } => {
                    self.execute_branch_f32_le(lhs, rhs, offset)
                }
                Instr::BranchF32Gt { lhs, rhs, offset } => {
                    self.execute_branch_f32_gt(lhs, rhs, offset)
                }
                Instr::BranchF32Ge { lhs, rhs, offset } => {
                    self.execute_branch_f32_ge(lhs, rhs, offset)
                }
                Instr::BranchF64Eq { lhs, rhs, offset } => {
                    self.execute_branch_f64_eq(lhs, rhs, offset)
                }
                Instr::BranchF64Ne { lhs, rhs, offset } => {
                    self.execute_branch_f64_ne(lhs, rhs, offset)
                }
                Instr::BranchF64Lt { lhs, rhs, offset } => {
                    self.execute_branch_f64_lt(lhs, rhs, offset)
                }
                Instr::BranchF64Le { lhs, rhs, offset } => {
                    self.execute_branch_f64_le(lhs, rhs, offset)
                }
                Instr::BranchF64Gt { lhs, rhs, offset } => {
                    self.execute_branch_f64_gt(lhs, rhs, offset)
                }
                Instr::BranchF64Ge { lhs, rhs, offset } => {
                    self.execute_branch_f64_ge(lhs, rhs, offset)
                }
                Instr::Copy { result, value } => self.execute_copy(result, value),
                Instr::Copy2 { results, values } => self.execute_copy_2(results, values),
                Instr::CopyImm32 { result, value } => self.execute_copy_imm32(result, value),
                Instr::CopyI64Imm32 { result, value } => self.execute_copy_i64imm32(result, value),
                Instr::CopyF64Imm32 { result, value } => self.execute_copy_f64imm32(result, value),
                Instr::CopySpan {
                    results,
                    values,
                    len,
                } => self.execute_copy_span(results, values, len),
                Instr::CopySpanNonOverlapping {
                    results,
                    values,
                    len,
                } => self.execute_copy_span_non_overlapping(results, values, len),
                Instr::CopyMany { results, values } => self.execute_copy_many(results, values),
                Instr::CopyManyNonOverlapping { results, values } => {
                    self.execute_copy_many_non_overlapping(results, values)
                }
                Instr::ReturnCallInternal0 { func } => {
                    self.execute_return_call_internal_0(&mut store.inner, func)?
                }
                Instr::ReturnCallInternal { func } => {
                    self.execute_return_call_internal(&mut store.inner, func)?
                }
                Instr::ReturnCallImported0 { func } => {
                    self.execute_return_call_imported_0::<T>(store, func)?
                }
                Instr::ReturnCallImported { func } => {
                    self.execute_return_call_imported::<T>(store, func)?
                }
                Instr::ReturnCallIndirect0 { func_type } => {
                    self.execute_return_call_indirect_0::<T>(store, func_type)?
                }
                Instr::ReturnCallIndirect0Imm16 { func_type } => {
                    self.execute_return_call_indirect_0_imm16::<T>(store, func_type)?
                }
                Instr::ReturnCallIndirect { func_type } => {
                    self.execute_return_call_indirect::<T>(store, func_type)?
                }
                Instr::ReturnCallIndirectImm16 { func_type } => {
                    self.execute_return_call_indirect_imm16::<T>(store, func_type)?
                }
                Instr::CallInternal0 { results, func } => {
                    self.execute_call_internal_0(&mut store.inner, results, func)?
                }
                Instr::CallInternal { results, func } => {
                    self.execute_call_internal(&mut store.inner, results, func)?
                }
                Instr::CallImported0 { results, func } => {
                    self.execute_call_imported_0::<T>(store, results, func)?
                }
                Instr::CallImported { results, func } => {
                    self.execute_call_imported::<T>(store, results, func)?
                }
                Instr::CallIndirect0 { results, func_type } => {
                    self.execute_call_indirect_0::<T>(store, results, func_type)?
                }
                Instr::CallIndirect0Imm16 { results, func_type } => {
                    self.execute_call_indirect_0_imm16::<T>(store, results, func_type)?
                }
                Instr::CallIndirect { results, func_type } => {
                    self.execute_call_indirect::<T>(store, results, func_type)?
                }
                Instr::CallIndirectImm16 { results, func_type } => {
                    self.execute_call_indirect_imm16::<T>(store, results, func_type)?
                }
                Instr::Select { result, lhs } => self.execute_select(result, lhs),
                Instr::SelectImm32Rhs { result, lhs } => self.execute_select_imm32_rhs(result, lhs),
                Instr::SelectImm32Lhs { result, lhs } => self.execute_select_imm32_lhs(result, lhs),
                Instr::SelectImm32 { result, lhs } => self.execute_select_imm32(result, lhs),
                Instr::SelectI64Imm32Rhs { result, lhs } => {
                    self.execute_select_i64imm32_rhs(result, lhs)
                }
                Instr::SelectI64Imm32Lhs { result, lhs } => {
                    self.execute_select_i64imm32_lhs(result, lhs)
                }
                Instr::SelectI64Imm32 { result, lhs } => self.execute_select_i64imm32(result, lhs),
                Instr::SelectF64Imm32Rhs { result, lhs } => {
                    self.execute_select_f64imm32_rhs(result, lhs)
                }
                Instr::SelectF64Imm32Lhs { result, lhs } => {
                    self.execute_select_f64imm32_lhs(result, lhs)
                }
                Instr::SelectF64Imm32 { result, lhs } => self.execute_select_f64imm32(result, lhs),
                Instr::RefFunc { result, func } => self.execute_ref_func(result, func),
                Instr::GlobalGet { result, global } => {
                    self.execute_global_get(&store.inner, result, global)
                }
                Instr::GlobalSet { global, input } => {
                    self.execute_global_set(&mut store.inner, global, input)
                }
                Instr::GlobalSetI32Imm16 { global, input } => {
                    self.execute_global_set_i32imm16(&mut store.inner, global, input)
                }
                Instr::GlobalSetI64Imm16 { global, input } => {
                    self.execute_global_set_i64imm16(&mut store.inner, global, input)
                }
                Instr::I32Load { result, ptr } => self.execute_i32_load(result, ptr)?,
                Instr::I32LoadAt { result, address } => {
                    self.execute_i32_load_at(result, address)?
                }
                Instr::I32LoadOffset16 {
                    result,
                    ptr,
                    offset,
                } => self.execute_i32_load_offset16(result, ptr, offset)?,
                Instr::I64Load { result, ptr } => self.execute_i64_load(result, ptr)?,
                Instr::I64LoadAt { result, address } => {
                    self.execute_i64_load_at(result, address)?
                }
                Instr::I64LoadOffset16 {
                    result,
                    ptr,
                    offset,
                } => self.execute_i64_load_offset16(result, ptr, offset)?,
                Instr::F32Load { result, ptr } => self.execute_f32_load(result, ptr)?,
                Instr::F32LoadAt { result, address } => {
                    self.execute_f32_load_at(result, address)?
                }
                Instr::F32LoadOffset16 {
                    result,
                    ptr,
                    offset,
                } => self.execute_f32_load_offset16(result, ptr, offset)?,
                Instr::F64Load { result, ptr } => self.execute_f64_load(result, ptr)?,
                Instr::F64LoadAt { result, address } => {
                    self.execute_f64_load_at(result, address)?
                }
                Instr::F64LoadOffset16 {
                    result,
                    ptr,
                    offset,
                } => self.execute_f64_load_offset16(result, ptr, offset)?,
                Instr::I32Load8s { result, ptr } => self.execute_i32_load8_s(result, ptr)?,
                Instr::I32Load8sAt { result, address } => {
                    self.execute_i32_load8_s_at(result, address)?
                }
                Instr::I32Load8sOffset16 {
                    result,
                    ptr,
                    offset,
                } => self.execute_i32_load8_s_offset16(result, ptr, offset)?,
                Instr::I32Load8u { result, ptr } => self.execute_i32_load8_u(result, ptr)?,
                Instr::I32Load8uAt { result, address } => {
                    self.execute_i32_load8_u_at(result, address)?
                }
                Instr::I32Load8uOffset16 {
                    result,
                    ptr,
                    offset,
                } => self.execute_i32_load8_u_offset16(result, ptr, offset)?,
                Instr::I32Load16s { result, ptr } => self.execute_i32_load16_s(result, ptr)?,
                Instr::I32Load16sAt { result, address } => {
                    self.execute_i32_load16_s_at(result, address)?
                }
                Instr::I32Load16sOffset16 {
                    result,
                    ptr,
                    offset,
                } => self.execute_i32_load16_s_offset16(result, ptr, offset)?,
                Instr::I32Load16u { result, ptr } => self.execute_i32_load16_u(result, ptr)?,
                Instr::I32Load16uAt { result, address } => {
                    self.execute_i32_load16_u_at(result, address)?
                }
                Instr::I32Load16uOffset16 {
                    result,
                    ptr,
                    offset,
                } => self.execute_i32_load16_u_offset16(result, ptr, offset)?,
                Instr::I64Load8s { result, ptr } => self.execute_i64_load8_s(result, ptr)?,
                Instr::I64Load8sAt { result, address } => {
                    self.execute_i64_load8_s_at(result, address)?
                }
                Instr::I64Load8sOffset16 {
                    result,
                    ptr,
                    offset,
                } => self.execute_i64_load8_s_offset16(result, ptr, offset)?,
                Instr::I64Load8u { result, ptr } => self.execute_i64_load8_u(result, ptr)?,
                Instr::I64Load8uAt { result, address } => {
                    self.execute_i64_load8_u_at(result, address)?
                }
                Instr::I64Load8uOffset16 {
                    result,
                    ptr,
                    offset,
                } => self.execute_i64_load8_u_offset16(result, ptr, offset)?,
                Instr::I64Load16s { result, ptr } => self.execute_i64_load16_s(result, ptr)?,
                Instr::I64Load16sAt { result, address } => {
                    self.execute_i64_load16_s_at(result, address)?
                }
                Instr::I64Load16sOffset16 {
                    result,
                    ptr,
                    offset,
                } => self.execute_i64_load16_s_offset16(result, ptr, offset)?,
                Instr::I64Load16u { result, ptr } => self.execute_i64_load16_u(result, ptr)?,
                Instr::I64Load16uAt { result, address } => {
                    self.execute_i64_load16_u_at(result, address)?
                }
                Instr::I64Load16uOffset16 {
                    result,
                    ptr,
                    offset,
                } => self.execute_i64_load16_u_offset16(result, ptr, offset)?,
                Instr::I64Load32s { result, ptr } => self.execute_i64_load32_s(result, ptr)?,
                Instr::I64Load32sAt { result, address } => {
                    self.execute_i64_load32_s_at(result, address)?
                }
                Instr::I64Load32sOffset16 {
                    result,
                    ptr,
                    offset,
                } => self.execute_i64_load32_s_offset16(result, ptr, offset)?,
                Instr::I64Load32u { result, ptr } => self.execute_i64_load32_u(result, ptr)?,
                Instr::I64Load32uAt { result, address } => {
                    self.execute_i64_load32_u_at(result, address)?
                }
                Instr::I64Load32uOffset16 {
                    result,
                    ptr,
                    offset,
                } => self.execute_i64_load32_u_offset16(result, ptr, offset)?,
                Instr::I32Store { ptr, offset } => self.execute_i32_store(ptr, offset)?,
                Instr::I32StoreOffset16 { ptr, offset, value } => {
                    self.execute_i32_store_offset16(ptr, offset, value)?
                }
                Instr::I32StoreOffset16Imm16 { ptr, offset, value } => {
                    self.execute_i32_store_offset16_imm16(ptr, offset, value)?
                }
                Instr::I32StoreAt { address, value } => {
                    self.execute_i32_store_at(address, value)?
                }
                Instr::I32StoreAtImm16 { address, value } => {
                    self.execute_i32_store_at_imm16(address, value)?
                }
                Instr::I32Store8 { ptr, offset } => self.execute_i32_store8(ptr, offset)?,
                Instr::I32Store8Offset16 { ptr, offset, value } => {
                    self.execute_i32_store8_offset16(ptr, offset, value)?
                }
                Instr::I32Store8Offset16Imm { ptr, offset, value } => {
                    self.execute_i32_store8_offset16_imm(ptr, offset, value)?
                }
                Instr::I32Store8At { address, value } => {
                    self.execute_i32_store8_at(address, value)?
                }
                Instr::I32Store8AtImm { address, value } => {
                    self.execute_i32_store8_at_imm(address, value)?
                }
                Instr::I32Store16 { ptr, offset } => self.execute_i32_store16(ptr, offset)?,
                Instr::I32Store16Offset16 { ptr, offset, value } => {
                    self.execute_i32_store16_offset16(ptr, offset, value)?
                }
                Instr::I32Store16Offset16Imm { ptr, offset, value } => {
                    self.execute_i32_store16_offset16_imm(ptr, offset, value)?
                }
                Instr::I32Store16At { address, value } => {
                    self.execute_i32_store16_at(address, value)?
                }
                Instr::I32Store16AtImm { address, value } => {
                    self.execute_i32_store16_at_imm(address, value)?
                }
                Instr::I64Store { ptr, offset } => self.execute_i64_store(ptr, offset)?,
                Instr::I64StoreOffset16 { ptr, offset, value } => {
                    self.execute_i64_store_offset16(ptr, offset, value)?
                }
                Instr::I64StoreOffset16Imm16 { ptr, offset, value } => {
                    self.execute_i64_store_offset16_imm16(ptr, offset, value)?
                }
                Instr::I64StoreAt { address, value } => {
                    self.execute_i64_store_at(address, value)?
                }
                Instr::I64StoreAtImm16 { address, value } => {
                    self.execute_i64_store_at_imm16(address, value)?
                }
                Instr::I64Store8 { ptr, offset } => self.execute_i64_store8(ptr, offset)?,
                Instr::I64Store8Offset16 { ptr, offset, value } => {
                    self.execute_i64_store8_offset16(ptr, offset, value)?
                }
                Instr::I64Store8Offset16Imm { ptr, offset, value } => {
                    self.execute_i64_store8_offset16_imm(ptr, offset, value)?
                }
                Instr::I64Store8At { address, value } => {
                    self.execute_i64_store8_at(address, value)?
                }
                Instr::I64Store8AtImm { address, value } => {
                    self.execute_i64_store8_at_imm(address, value)?
                }
                Instr::I64Store16 { ptr, offset } => self.execute_i64_store16(ptr, offset)?,
                Instr::I64Store16Offset16 { ptr, offset, value } => {
                    self.execute_i64_store16_offset16(ptr, offset, value)?
                }
                Instr::I64Store16Offset16Imm { ptr, offset, value } => {
                    self.execute_i64_store16_offset16_imm(ptr, offset, value)?
                }
                Instr::I64Store16At { address, value } => {
                    self.execute_i64_store16_at(address, value)?
                }
                Instr::I64Store16AtImm { address, value } => {
                    self.execute_i64_store16_at_imm(address, value)?
                }
                Instr::I64Store32 { ptr, offset } => self.execute_i64_store32(ptr, offset)?,
                Instr::I64Store32Offset16 { ptr, offset, value } => {
                    self.execute_i64_store32_offset16(ptr, offset, value)?
                }
                Instr::I64Store32Offset16Imm16 { ptr, offset, value } => {
                    self.execute_i64_store32_offset16_imm16(ptr, offset, value)?
                }
                Instr::I64Store32At { address, value } => {
                    self.execute_i64_store32_at(address, value)?
                }
                Instr::I64Store32AtImm16 { address, value } => {
                    self.execute_i64_store32_at_imm16(address, value)?
                }
                Instr::F32Store { ptr, offset } => self.execute_f32_store(ptr, offset)?,
                Instr::F32StoreOffset16 { ptr, offset, value } => {
                    self.execute_f32_store_offset16(ptr, offset, value)?
                }
                Instr::F32StoreAt { address, value } => {
                    self.execute_f32_store_at(address, value)?
                }
                Instr::F64Store { ptr, offset } => self.execute_f64_store(ptr, offset)?,
                Instr::F64StoreOffset16 { ptr, offset, value } => {
                    self.execute_f64_store_offset16(ptr, offset, value)?
                }
                Instr::F64StoreAt { address, value } => {
                    self.execute_f64_store_at(address, value)?
                }
                Instr::I32Eq { result, lhs, rhs } => self.execute_i32_eq(result, lhs, rhs),
                Instr::I32EqImm16 { result, lhs, rhs } => {
                    self.execute_i32_eq_imm16(result, lhs, rhs)
                }
                Instr::I32Ne { result, lhs, rhs } => self.execute_i32_ne(result, lhs, rhs),
                Instr::I32NeImm16 { result, lhs, rhs } => {
                    self.execute_i32_ne_imm16(result, lhs, rhs)
                }
                Instr::I32LtS { result, lhs, rhs } => self.execute_i32_lt_s(result, lhs, rhs),
                Instr::I32LtSImm16 { result, lhs, rhs } => {
                    self.execute_i32_lt_s_imm16(result, lhs, rhs)
                }
                Instr::I32LtU { result, lhs, rhs } => self.execute_i32_lt_u(result, lhs, rhs),
                Instr::I32LtUImm16 { result, lhs, rhs } => {
                    self.execute_i32_lt_u_imm16(result, lhs, rhs)
                }
                Instr::I32LeS { result, lhs, rhs } => self.execute_i32_le_s(result, lhs, rhs),
                Instr::I32LeSImm16 { result, lhs, rhs } => {
                    self.execute_i32_le_s_imm16(result, lhs, rhs)
                }
                Instr::I32LeU { result, lhs, rhs } => self.execute_i32_le_u(result, lhs, rhs),
                Instr::I32LeUImm16 { result, lhs, rhs } => {
                    self.execute_i32_le_u_imm16(result, lhs, rhs)
                }
                Instr::I32GtS { result, lhs, rhs } => self.execute_i32_gt_s(result, lhs, rhs),
                Instr::I32GtSImm16 { result, lhs, rhs } => {
                    self.execute_i32_gt_s_imm16(result, lhs, rhs)
                }
                Instr::I32GtU { result, lhs, rhs } => self.execute_i32_gt_u(result, lhs, rhs),
                Instr::I32GtUImm16 { result, lhs, rhs } => {
                    self.execute_i32_gt_u_imm16(result, lhs, rhs)
                }
                Instr::I32GeS { result, lhs, rhs } => self.execute_i32_ge_s(result, lhs, rhs),
                Instr::I32GeSImm16 { result, lhs, rhs } => {
                    self.execute_i32_ge_s_imm16(result, lhs, rhs)
                }
                Instr::I32GeU { result, lhs, rhs } => self.execute_i32_ge_u(result, lhs, rhs),
                Instr::I32GeUImm16 { result, lhs, rhs } => {
                    self.execute_i32_ge_u_imm16(result, lhs, rhs)
                }
                Instr::I64Eq { result, lhs, rhs } => self.execute_i64_eq(result, lhs, rhs),
                Instr::I64EqImm16 { result, lhs, rhs } => {
                    self.execute_i64_eq_imm16(result, lhs, rhs)
                }
                Instr::I64Ne { result, lhs, rhs } => self.execute_i64_ne(result, lhs, rhs),
                Instr::I64NeImm16 { result, lhs, rhs } => {
                    self.execute_i64_ne_imm16(result, lhs, rhs)
                }
                Instr::I64LtS { result, lhs, rhs } => self.execute_i64_lt_s(result, lhs, rhs),
                Instr::I64LtSImm16 { result, lhs, rhs } => {
                    self.execute_i64_lt_s_imm16(result, lhs, rhs)
                }
                Instr::I64LtU { result, lhs, rhs } => self.execute_i64_lt_u(result, lhs, rhs),
                Instr::I64LtUImm16 { result, lhs, rhs } => {
                    self.execute_i64_lt_u_imm16(result, lhs, rhs)
                }
                Instr::I64LeS { result, lhs, rhs } => self.execute_i64_le_s(result, lhs, rhs),
                Instr::I64LeSImm16 { result, lhs, rhs } => {
                    self.execute_i64_le_s_imm16(result, lhs, rhs)
                }
                Instr::I64LeU { result, lhs, rhs } => self.execute_i64_le_u(result, lhs, rhs),
                Instr::I64LeUImm16 { result, lhs, rhs } => {
                    self.execute_i64_le_u_imm16(result, lhs, rhs)
                }
                Instr::I64GtS { result, lhs, rhs } => self.execute_i64_gt_s(result, lhs, rhs),
                Instr::I64GtSImm16 { result, lhs, rhs } => {
                    self.execute_i64_gt_s_imm16(result, lhs, rhs)
                }
                Instr::I64GtU { result, lhs, rhs } => self.execute_i64_gt_u(result, lhs, rhs),
                Instr::I64GtUImm16 { result, lhs, rhs } => {
                    self.execute_i64_gt_u_imm16(result, lhs, rhs)
                }
                Instr::I64GeS { result, lhs, rhs } => self.execute_i64_ge_s(result, lhs, rhs),
                Instr::I64GeSImm16 { result, lhs, rhs } => {
                    self.execute_i64_ge_s_imm16(result, lhs, rhs)
                }
                Instr::I64GeU { result, lhs, rhs } => self.execute_i64_ge_u(result, lhs, rhs),
                Instr::I64GeUImm16 { result, lhs, rhs } => {
                    self.execute_i64_ge_u_imm16(result, lhs, rhs)
                }
                Instr::F32Eq { result, lhs, rhs } => self.execute_f32_eq(result, lhs, rhs),
                Instr::F32Ne { result, lhs, rhs } => self.execute_f32_ne(result, lhs, rhs),
                Instr::F32Lt { result, lhs, rhs } => self.execute_f32_lt(result, lhs, rhs),
                Instr::F32Le { result, lhs, rhs } => self.execute_f32_le(result, lhs, rhs),
                Instr::F32Gt { result, lhs, rhs } => self.execute_f32_gt(result, lhs, rhs),
                Instr::F32Ge { result, lhs, rhs } => self.execute_f32_ge(result, lhs, rhs),
                Instr::F64Eq { result, lhs, rhs } => self.execute_f64_eq(result, lhs, rhs),
                Instr::F64Ne { result, lhs, rhs } => self.execute_f64_ne(result, lhs, rhs),
                Instr::F64Lt { result, lhs, rhs } => self.execute_f64_lt(result, lhs, rhs),
                Instr::F64Le { result, lhs, rhs } => self.execute_f64_le(result, lhs, rhs),
                Instr::F64Gt { result, lhs, rhs } => self.execute_f64_gt(result, lhs, rhs),
                Instr::F64Ge { result, lhs, rhs } => self.execute_f64_ge(result, lhs, rhs),
                Instr::I32Clz { result, input } => self.execute_i32_clz(result, input),
                Instr::I32Ctz { result, input } => self.execute_i32_ctz(result, input),
                Instr::I32Popcnt { result, input } => self.execute_i32_popcnt(result, input),
                Instr::I32Add { result, lhs, rhs } => self.execute_i32_add(result, lhs, rhs),
                Instr::I32AddImm16 { result, lhs, rhs } => {
                    self.execute_i32_add_imm16(result, lhs, rhs)
                }
                Instr::I32Sub { result, lhs, rhs } => self.execute_i32_sub(result, lhs, rhs),
                Instr::I32SubImm16Rev { result, lhs, rhs } => {
                    self.execute_i32_sub_imm16_rev(result, lhs, rhs)
                }
                Instr::I32Mul { result, lhs, rhs } => self.execute_i32_mul(result, lhs, rhs),
                Instr::I32MulImm16 { result, lhs, rhs } => {
                    self.execute_i32_mul_imm16(result, lhs, rhs)
                }
                Instr::I32DivS { result, lhs, rhs } => self.execute_i32_div_s(result, lhs, rhs)?,
                Instr::I32DivSImm16 { result, lhs, rhs } => {
                    self.execute_i32_div_s_imm16(result, lhs, rhs)?
                }
                Instr::I32DivSImm16Rev { result, lhs, rhs } => {
                    self.execute_i32_div_s_imm16_rev(result, lhs, rhs)?
                }
                Instr::I32DivU { result, lhs, rhs } => self.execute_i32_div_u(result, lhs, rhs)?,
                Instr::I32DivUImm16 { result, lhs, rhs } => {
                    self.execute_i32_div_u_imm16(result, lhs, rhs)
                }
                Instr::I32DivUImm16Rev { result, lhs, rhs } => {
                    self.execute_i32_div_u_imm16_rev(result, lhs, rhs)?
                }
                Instr::I32RemS { result, lhs, rhs } => self.execute_i32_rem_s(result, lhs, rhs)?,
                Instr::I32RemSImm16 { result, lhs, rhs } => {
                    self.execute_i32_rem_s_imm16(result, lhs, rhs)?
                }
                Instr::I32RemSImm16Rev { result, lhs, rhs } => {
                    self.execute_i32_rem_s_imm16_rev(result, lhs, rhs)?
                }
                Instr::I32RemU { result, lhs, rhs } => self.execute_i32_rem_u(result, lhs, rhs)?,
                Instr::I32RemUImm16 { result, lhs, rhs } => {
                    self.execute_i32_rem_u_imm16(result, lhs, rhs)
                }
                Instr::I32RemUImm16Rev { result, lhs, rhs } => {
                    self.execute_i32_rem_u_imm16_rev(result, lhs, rhs)?
                }
                Instr::I32And { result, lhs, rhs } => self.execute_i32_and(result, lhs, rhs),
                Instr::I32AndEqz { result, lhs, rhs } => self.execute_i32_and_eqz(result, lhs, rhs),
                Instr::I32AndEqzImm16 { result, lhs, rhs } => {
                    self.execute_i32_and_eqz_imm16(result, lhs, rhs)
                }
                Instr::I32AndImm16 { result, lhs, rhs } => {
                    self.execute_i32_and_imm16(result, lhs, rhs)
                }
                Instr::I32Or { result, lhs, rhs } => self.execute_i32_or(result, lhs, rhs),
                Instr::I32OrEqz { result, lhs, rhs } => self.execute_i32_or_eqz(result, lhs, rhs),
                Instr::I32OrEqzImm16 { result, lhs, rhs } => {
                    self.execute_i32_or_eqz_imm16(result, lhs, rhs)
                }
                Instr::I32OrImm16 { result, lhs, rhs } => {
                    self.execute_i32_or_imm16(result, lhs, rhs)
                }
                Instr::I32Xor { result, lhs, rhs } => self.execute_i32_xor(result, lhs, rhs),
                Instr::I32XorEqz { result, lhs, rhs } => self.execute_i32_xor_eqz(result, lhs, rhs),
                Instr::I32XorEqzImm16 { result, lhs, rhs } => {
                    self.execute_i32_xor_eqz_imm16(result, lhs, rhs)
                }
                Instr::I32XorImm16 { result, lhs, rhs } => {
                    self.execute_i32_xor_imm16(result, lhs, rhs)
                }
                Instr::I32Shl { result, lhs, rhs } => self.execute_i32_shl(result, lhs, rhs),
                Instr::I32ShlImm { result, lhs, rhs } => self.execute_i32_shl_imm(result, lhs, rhs),
                Instr::I32ShlImm16Rev { result, lhs, rhs } => {
                    self.execute_i32_shl_imm16_rev(result, lhs, rhs)
                }
                Instr::I32ShrU { result, lhs, rhs } => self.execute_i32_shr_u(result, lhs, rhs),
                Instr::I32ShrUImm { result, lhs, rhs } => {
                    self.execute_i32_shr_u_imm(result, lhs, rhs)
                }
                Instr::I32ShrUImm16Rev { result, lhs, rhs } => {
                    self.execute_i32_shr_u_imm16_rev(result, lhs, rhs)
                }
                Instr::I32ShrS { result, lhs, rhs } => self.execute_i32_shr_s(result, lhs, rhs),
                Instr::I32ShrSImm { result, lhs, rhs } => {
                    self.execute_i32_shr_s_imm(result, lhs, rhs)
                }
                Instr::I32ShrSImm16Rev { result, lhs, rhs } => {
                    self.execute_i32_shr_s_imm16_rev(result, lhs, rhs)
                }
                Instr::I32Rotl { result, lhs, rhs } => self.execute_i32_rotl(result, lhs, rhs),
                Instr::I32RotlImm { result, lhs, rhs } => {
                    self.execute_i32_rotl_imm(result, lhs, rhs)
                }
                Instr::I32RotlImm16Rev { result, lhs, rhs } => {
                    self.execute_i32_rotl_imm16_rev(result, lhs, rhs)
                }
                Instr::I32Rotr { result, lhs, rhs } => self.execute_i32_rotr(result, lhs, rhs),
                Instr::I32RotrImm { result, lhs, rhs } => {
                    self.execute_i32_rotr_imm(result, lhs, rhs)
                }
                Instr::I32RotrImm16Rev { result, lhs, rhs } => {
                    self.execute_i32_rotr_imm16_rev(result, lhs, rhs)
                }
                Instr::I64Clz { result, input } => self.execute_i64_clz(result, input),
                Instr::I64Ctz { result, input } => self.execute_i64_ctz(result, input),
                Instr::I64Popcnt { result, input } => self.execute_i64_popcnt(result, input),
                Instr::I64Add { result, lhs, rhs } => self.execute_i64_add(result, lhs, rhs),
                Instr::I64AddImm16 { result, lhs, rhs } => {
                    self.execute_i64_add_imm16(result, lhs, rhs)
                }
                Instr::I64Sub { result, lhs, rhs } => self.execute_i64_sub(result, lhs, rhs),
                Instr::I64SubImm16Rev { result, lhs, rhs } => {
                    self.execute_i64_sub_imm16_rev(result, lhs, rhs)
                }
                Instr::I64Mul { result, lhs, rhs } => self.execute_i64_mul(result, lhs, rhs),
                Instr::I64MulImm16 { result, lhs, rhs } => {
                    self.execute_i64_mul_imm16(result, lhs, rhs)
                }
                Instr::I64DivS { result, lhs, rhs } => self.execute_i64_div_s(result, lhs, rhs)?,
                Instr::I64DivSImm16 { result, lhs, rhs } => {
                    self.execute_i64_div_s_imm16(result, lhs, rhs)?
                }
                Instr::I64DivSImm16Rev { result, lhs, rhs } => {
                    self.execute_i64_div_s_imm16_rev(result, lhs, rhs)?
                }
                Instr::I64DivU { result, lhs, rhs } => self.execute_i64_div_u(result, lhs, rhs)?,
                Instr::I64DivUImm16 { result, lhs, rhs } => {
                    self.execute_i64_div_u_imm16(result, lhs, rhs)
                }
                Instr::I64DivUImm16Rev { result, lhs, rhs } => {
                    self.execute_i64_div_u_imm16_rev(result, lhs, rhs)?
                }
                Instr::I64RemS { result, lhs, rhs } => self.execute_i64_rem_s(result, lhs, rhs)?,
                Instr::I64RemSImm16 { result, lhs, rhs } => {
                    self.execute_i64_rem_s_imm16(result, lhs, rhs)?
                }
                Instr::I64RemSImm16Rev { result, lhs, rhs } => {
                    self.execute_i64_rem_s_imm16_rev(result, lhs, rhs)?
                }
                Instr::I64RemU { result, lhs, rhs } => self.execute_i64_rem_u(result, lhs, rhs)?,
                Instr::I64RemUImm16 { result, lhs, rhs } => {
                    self.execute_i64_rem_u_imm16(result, lhs, rhs)
                }
                Instr::I64RemUImm16Rev { result, lhs, rhs } => {
                    self.execute_i64_rem_u_imm16_rev(result, lhs, rhs)?
                }
                Instr::I64And { result, lhs, rhs } => self.execute_i64_and(result, lhs, rhs),
                Instr::I64AndImm16 { result, lhs, rhs } => {
                    self.execute_i64_and_imm16(result, lhs, rhs)
                }
                Instr::I64Or { result, lhs, rhs } => self.execute_i64_or(result, lhs, rhs),
                Instr::I64OrImm16 { result, lhs, rhs } => {
                    self.execute_i64_or_imm16(result, lhs, rhs)
                }
                Instr::I64Xor { result, lhs, rhs } => self.execute_i64_xor(result, lhs, rhs),
                Instr::I64XorImm16 { result, lhs, rhs } => {
                    self.execute_i64_xor_imm16(result, lhs, rhs)
                }
                Instr::I64Shl { result, lhs, rhs } => self.execute_i64_shl(result, lhs, rhs),
                Instr::I64ShlImm { result, lhs, rhs } => self.execute_i64_shl_imm(result, lhs, rhs),
                Instr::I64ShlImm16Rev { result, lhs, rhs } => {
                    self.execute_i64_shl_imm16_rev(result, lhs, rhs)
                }
                Instr::I64ShrU { result, lhs, rhs } => self.execute_i64_shr_u(result, lhs, rhs),
                Instr::I64ShrUImm { result, lhs, rhs } => {
                    self.execute_i64_shr_u_imm(result, lhs, rhs)
                }
                Instr::I64ShrUImm16Rev { result, lhs, rhs } => {
                    self.execute_i64_shr_u_imm16_rev(result, lhs, rhs)
                }
                Instr::I64ShrS { result, lhs, rhs } => self.execute_i64_shr_s(result, lhs, rhs),
                Instr::I64ShrSImm { result, lhs, rhs } => {
                    self.execute_i64_shr_s_imm(result, lhs, rhs)
                }
                Instr::I64ShrSImm16Rev { result, lhs, rhs } => {
                    self.execute_i64_shr_s_imm16_rev(result, lhs, rhs)
                }
                Instr::I64Rotl { result, lhs, rhs } => self.execute_i64_rotl(result, lhs, rhs),
                Instr::I64RotlImm { result, lhs, rhs } => {
                    self.execute_i64_rotl_imm(result, lhs, rhs)
                }
                Instr::I64RotlImm16Rev { result, lhs, rhs } => {
                    self.execute_i64_rotl_imm16_rev(result, lhs, rhs)
                }
                Instr::I64Rotr { result, lhs, rhs } => self.execute_i64_rotr(result, lhs, rhs),
                Instr::I64RotrImm { result, lhs, rhs } => {
                    self.execute_i64_rotr_imm(result, lhs, rhs)
                }
                Instr::I64RotrImm16Rev { result, lhs, rhs } => {
                    self.execute_i64_rotr_imm16_rev(result, lhs, rhs)
                }
                Instr::I32WrapI64 { result, input } => self.execute_i32_wrap_i64(result, input),
                Instr::I32Extend8S { result, input } => self.execute_i32_extend8_s(result, input),
                Instr::I32Extend16S { result, input } => self.execute_i32_extend16_s(result, input),
                Instr::I64Extend8S { result, input } => self.execute_i64_extend8_s(result, input),
                Instr::I64Extend16S { result, input } => self.execute_i64_extend16_s(result, input),
                Instr::I64Extend32S { result, input } => self.execute_i64_extend32_s(result, input),
                Instr::F32Abs { result, input } => self.execute_f32_abs(result, input),
                Instr::F32Neg { result, input } => self.execute_f32_neg(result, input),
                Instr::F32Ceil { result, input } => self.execute_f32_ceil(result, input),
                Instr::F32Floor { result, input } => self.execute_f32_floor(result, input),
                Instr::F32Trunc { result, input } => self.execute_f32_trunc(result, input),
                Instr::F32Nearest { result, input } => self.execute_f32_nearest(result, input),
                Instr::F32Sqrt { result, input } => self.execute_f32_sqrt(result, input),
                Instr::F32Add { result, lhs, rhs } => self.execute_f32_add(result, lhs, rhs),
                Instr::F32Sub { result, lhs, rhs } => self.execute_f32_sub(result, lhs, rhs),
                Instr::F32Mul { result, lhs, rhs } => self.execute_f32_mul(result, lhs, rhs),
                Instr::F32Div { result, lhs, rhs } => self.execute_f32_div(result, lhs, rhs),
                Instr::F32Min { result, lhs, rhs } => self.execute_f32_min(result, lhs, rhs),
                Instr::F32Max { result, lhs, rhs } => self.execute_f32_max(result, lhs, rhs),
                Instr::F32Copysign { result, lhs, rhs } => {
                    self.execute_f32_copysign(result, lhs, rhs)
                }
                Instr::F32CopysignImm { result, lhs, rhs } => {
                    self.execute_f32_copysign_imm(result, lhs, rhs)
                }
                Instr::F64Abs { result, input } => self.execute_f64_abs(result, input),
                Instr::F64Neg { result, input } => self.execute_f64_neg(result, input),
                Instr::F64Ceil { result, input } => self.execute_f64_ceil(result, input),
                Instr::F64Floor { result, input } => self.execute_f64_floor(result, input),
                Instr::F64Trunc { result, input } => self.execute_f64_trunc(result, input),
                Instr::F64Nearest { result, input } => self.execute_f64_nearest(result, input),
                Instr::F64Sqrt { result, input } => self.execute_f64_sqrt(result, input),
                Instr::F64Add { result, lhs, rhs } => self.execute_f64_add(result, lhs, rhs),
                Instr::F64Sub { result, lhs, rhs } => self.execute_f64_sub(result, lhs, rhs),
                Instr::F64Mul { result, lhs, rhs } => self.execute_f64_mul(result, lhs, rhs),
                Instr::F64Div { result, lhs, rhs } => self.execute_f64_div(result, lhs, rhs),
                Instr::F64Min { result, lhs, rhs } => self.execute_f64_min(result, lhs, rhs),
                Instr::F64Max { result, lhs, rhs } => self.execute_f64_max(result, lhs, rhs),
                Instr::F64Copysign { result, lhs, rhs } => {
                    self.execute_f64_copysign(result, lhs, rhs)
                }
                Instr::F64CopysignImm { result, lhs, rhs } => {
                    self.execute_f64_copysign_imm(result, lhs, rhs)
                }
                Instr::I32TruncF32S { result, input } => {
                    self.execute_i32_trunc_f32_s(result, input)?
                }
                Instr::I32TruncF32U { result, input } => {
                    self.execute_i32_trunc_f32_u(result, input)?
                }
                Instr::I32TruncF64S { result, input } => {
                    self.execute_i32_trunc_f64_s(result, input)?
                }
                Instr::I32TruncF64U { result, input } => {
                    self.execute_i32_trunc_f64_u(result, input)?
                }
                Instr::I64TruncF32S { result, input } => {
                    self.execute_i64_trunc_f32_s(result, input)?
                }
                Instr::I64TruncF32U { result, input } => {
                    self.execute_i64_trunc_f32_u(result, input)?
                }
                Instr::I64TruncF64S { result, input } => {
                    self.execute_i64_trunc_f64_s(result, input)?
                }
                Instr::I64TruncF64U { result, input } => {
                    self.execute_i64_trunc_f64_u(result, input)?
                }
                Instr::I32TruncSatF32S { result, input } => {
                    self.execute_i32_trunc_sat_f32_s(result, input)
                }
                Instr::I32TruncSatF32U { result, input } => {
                    self.execute_i32_trunc_sat_f32_u(result, input)
                }
                Instr::I32TruncSatF64S { result, input } => {
                    self.execute_i32_trunc_sat_f64_s(result, input)
                }
                Instr::I32TruncSatF64U { result, input } => {
                    self.execute_i32_trunc_sat_f64_u(result, input)
                }
                Instr::I64TruncSatF32S { result, input } => {
                    self.execute_i64_trunc_sat_f32_s(result, input)
                }
                Instr::I64TruncSatF32U { result, input } => {
                    self.execute_i64_trunc_sat_f32_u(result, input)
                }
                Instr::I64TruncSatF64S { result, input } => {
                    self.execute_i64_trunc_sat_f64_s(result, input)
                }
                Instr::I64TruncSatF64U { result, input } => {
                    self.execute_i64_trunc_sat_f64_u(result, input)
                }
                Instr::F32DemoteF64 { result, input } => self.execute_f32_demote_f64(result, input),
                Instr::F64PromoteF32 { result, input } => {
                    self.execute_f64_promote_f32(result, input)
                }
                Instr::F32ConvertI32S { result, input } => {
                    self.execute_f32_convert_i32_s(result, input)
                }
                Instr::F32ConvertI32U { result, input } => {
                    self.execute_f32_convert_i32_u(result, input)
                }
                Instr::F32ConvertI64S { result, input } => {
                    self.execute_f32_convert_i64_s(result, input)
                }
                Instr::F32ConvertI64U { result, input } => {
                    self.execute_f32_convert_i64_u(result, input)
                }
                Instr::F64ConvertI32S { result, input } => {
                    self.execute_f64_convert_i32_s(result, input)
                }
                Instr::F64ConvertI32U { result, input } => {
                    self.execute_f64_convert_i32_u(result, input)
                }
                Instr::F64ConvertI64S { result, input } => {
                    self.execute_f64_convert_i64_s(result, input)
                }
                Instr::F64ConvertI64U { result, input } => {
                    self.execute_f64_convert_i64_u(result, input)
                }
                Instr::TableGet { result, index } => {
                    self.execute_table_get(&store.inner, result, index)?
                }
                Instr::TableGetImm { result, index } => {
                    self.execute_table_get_imm(&store.inner, result, index)?
                }
                Instr::TableSize { result, table } => {
                    self.execute_table_size(&store.inner, result, table)
                }
                Instr::TableSet { index, value } => {
                    self.execute_table_set(&mut store.inner, index, value)?
                }
                Instr::TableSetAt { index, value } => {
                    self.execute_table_set_at(&mut store.inner, index, value)?
                }
                Instr::TableCopy { dst, src, len } => {
                    self.execute_table_copy(&mut store.inner, dst, src, len)?
                }
                Instr::TableCopyTo { dst, src, len } => {
                    self.execute_table_copy_to(&mut store.inner, dst, src, len)?
                }
                Instr::TableCopyFrom { dst, src, len } => {
                    self.execute_table_copy_from(&mut store.inner, dst, src, len)?
                }
                Instr::TableCopyFromTo { dst, src, len } => {
                    self.execute_table_copy_from_to(&mut store.inner, dst, src, len)?
                }
                Instr::TableCopyExact { dst, src, len } => {
                    self.execute_table_copy_exact(&mut store.inner, dst, src, len)?
                }
                Instr::TableCopyToExact { dst, src, len } => {
                    self.execute_table_copy_to_exact(&mut store.inner, dst, src, len)?
                }
                Instr::TableCopyFromExact { dst, src, len } => {
                    self.execute_table_copy_from_exact(&mut store.inner, dst, src, len)?
                }
                Instr::TableCopyFromToExact { dst, src, len } => {
                    self.execute_table_copy_from_to_exact(&mut store.inner, dst, src, len)?
                }
                Instr::TableInit { dst, src, len } => {
                    self.execute_table_init(&mut store.inner, dst, src, len)?
                }
                Instr::TableInitTo { dst, src, len } => {
                    self.execute_table_init_to(&mut store.inner, dst, src, len)?
                }
                Instr::TableInitFrom { dst, src, len } => {
                    self.execute_table_init_from(&mut store.inner, dst, src, len)?
                }
                Instr::TableInitFromTo { dst, src, len } => {
                    self.execute_table_init_from_to(&mut store.inner, dst, src, len)?
                }
                Instr::TableInitExact { dst, src, len } => {
                    self.execute_table_init_exact(&mut store.inner, dst, src, len)?
                }
                Instr::TableInitToExact { dst, src, len } => {
                    self.execute_table_init_to_exact(&mut store.inner, dst, src, len)?
                }
                Instr::TableInitFromExact { dst, src, len } => {
                    self.execute_table_init_from_exact(&mut store.inner, dst, src, len)?
                }
                Instr::TableInitFromToExact { dst, src, len } => {
                    self.execute_table_init_from_to_exact(&mut store.inner, dst, src, len)?
                }
                Instr::TableFill { dst, len, value } => {
                    self.execute_table_fill(&mut store.inner, dst, len, value)?
                }
                Instr::TableFillAt { dst, len, value } => {
                    self.execute_table_fill_at(&mut store.inner, dst, len, value)?
                }
                Instr::TableFillExact { dst, len, value } => {
                    self.execute_table_fill_exact(&mut store.inner, dst, len, value)?
                }
                Instr::TableFillAtExact { dst, len, value } => {
                    self.execute_table_fill_at_exact(&mut store.inner, dst, len, value)?
                }
                Instr::TableGrow {
                    result,
                    delta,
                    value,
                } => self.execute_table_grow(store, result, delta, value)?,
                Instr::TableGrowImm {
                    result,
                    delta,
                    value,
                } => self.execute_table_grow_imm(store, result, delta, value)?,
                Instr::ElemDrop(element_index) => {
                    self.execute_element_drop(&mut store.inner, element_index)
                }
                Instr::DataDrop(data_index) => self.execute_data_drop(&mut store.inner, data_index),
                Instr::MemorySize { result } => self.execute_memory_size(&store.inner, result),
                Instr::MemoryGrow { result, delta } => {
                    self.execute_memory_grow(store, result, delta)?
                }
                Instr::MemoryGrowBy { result, delta } => {
                    self.execute_memory_grow_by(store, result, delta)?
                }
                Instr::MemoryCopy { dst, src, len } => {
                    self.execute_memory_copy(&mut store.inner, dst, src, len)?
                }
                Instr::MemoryCopyTo { dst, src, len } => {
                    self.execute_memory_copy_to(&mut store.inner, dst, src, len)?
                }
                Instr::MemoryCopyFrom { dst, src, len } => {
                    self.execute_memory_copy_from(&mut store.inner, dst, src, len)?
                }
                Instr::MemoryCopyFromTo { dst, src, len } => {
                    self.execute_memory_copy_from_to(&mut store.inner, dst, src, len)?
                }
                Instr::MemoryCopyExact { dst, src, len } => {
                    self.execute_memory_copy_exact(&mut store.inner, dst, src, len)?
                }
                Instr::MemoryCopyToExact { dst, src, len } => {
                    self.execute_memory_copy_to_exact(&mut store.inner, dst, src, len)?
                }
                Instr::MemoryCopyFromExact { dst, src, len } => {
                    self.execute_memory_copy_from_exact(&mut store.inner, dst, src, len)?
                }
                Instr::MemoryCopyFromToExact { dst, src, len } => {
                    self.execute_memory_copy_from_to_exact(&mut store.inner, dst, src, len)?
                }
                Instr::MemoryFill { dst, value, len } => {
                    self.execute_memory_fill(&mut store.inner, dst, value, len)?
                }
                Instr::MemoryFillAt { dst, value, len } => {
                    self.execute_memory_fill_at(&mut store.inner, dst, value, len)?
                }
                Instr::MemoryFillImm { dst, value, len } => {
                    self.execute_memory_fill_imm(&mut store.inner, dst, value, len)?
                }
                Instr::MemoryFillExact { dst, value, len } => {
                    self.execute_memory_fill_exact(&mut store.inner, dst, value, len)?
                }
                Instr::MemoryFillAtImm { dst, value, len } => {
                    self.execute_memory_fill_at_imm(&mut store.inner, dst, value, len)?
                }
                Instr::MemoryFillAtExact { dst, value, len } => {
                    self.execute_memory_fill_at_exact(&mut store.inner, dst, value, len)?
                }
                Instr::MemoryFillImmExact { dst, value, len } => {
                    self.execute_memory_fill_imm_exact(&mut store.inner, dst, value, len)?
                }
                Instr::MemoryFillAtImmExact { dst, value, len } => {
                    self.execute_memory_fill_at_imm_exact(&mut store.inner, dst, value, len)?
                }
                Instr::MemoryInit { dst, src, len } => {
                    self.execute_memory_init(&mut store.inner, dst, src, len)?
                }
                Instr::MemoryInitTo { dst, src, len } => {
                    self.execute_memory_init_to(&mut store.inner, dst, src, len)?
                }
                Instr::MemoryInitFrom { dst, src, len } => {
                    self.execute_memory_init_from(&mut store.inner, dst, src, len)?
                }
                Instr::MemoryInitFromTo { dst, src, len } => {
                    self.execute_memory_init_from_to(&mut store.inner, dst, src, len)?
                }
                Instr::MemoryInitExact { dst, src, len } => {
                    self.execute_memory_init_exact(&mut store.inner, dst, src, len)?
                }
                Instr::MemoryInitToExact { dst, src, len } => {
                    self.execute_memory_init_to_exact(&mut store.inner, dst, src, len)?
                }
                Instr::MemoryInitFromExact { dst, src, len } => {
                    self.execute_memory_init_from_exact(&mut store.inner, dst, src, len)?
                }
                Instr::MemoryInitFromToExact { dst, src, len } => {
                    self.execute_memory_init_from_to_exact(&mut store.inner, dst, src, len)?
                }
                Instr::TableIdx(_)
                | Instr::DataSegmentIdx(_)
                | Instr::ElementSegmentIdx(_)
                | Instr::Const32(_)
                | Instr::I64Const32(_)
                | Instr::F64Const32(_)
                | Instr::BranchTableTarget { .. }
                | Instr::BranchTableTargetNonOverlapping { .. }
                | Instr::Register(_)
                | Instr::Register2(_)
                | Instr::Register3(_)
                | Instr::RegisterAndImm32 { .. }
                | Instr::RegisterSpan(_)
                | Instr::RegisterList(_)
                | Instr::CallIndirectParams(_)
                | Instr::CallIndirectParamsImm16(_) => self.invalid_instruction_word()?,
            }
        }
    }
}

macro_rules! get_entity {
    (
        $(
            fn $name:ident(&self, store: &StoreInner, index: $index_ty:ty) -> $id_ty:ty;
        )*
    ) => {
        $(
            #[doc = ::core::concat!(
                "Returns the [`",
                ::core::stringify!($id_ty),
                "`] at `index` for the currently used [`Instance`].\n\n",
                "# Panics\n\n",
                "- If there is no [`",
                ::core::stringify!($id_ty),
                "`] at `index` for the currently used [`Instance`] in `store`."
            )]
            #[inline]
            fn $name(&self, index: $index_ty) -> $id_ty {
                unsafe { self.cache.$name(index) }
                    .unwrap_or_else(|| {
                        const ENTITY_NAME: &'static str = ::core::stringify!($id_ty);
                        ::core::unreachable!(
                            "missing {ENTITY_NAME} at index {index:?} for the currently used instance",
                        )
                    })
            }
        )*
    }
}

impl<'engine> Executor<'engine> {
    get_entity! {
        fn get_func(&self, store: &StoreInner, index: FuncIdx) -> Func;
        fn get_func_type_dedup(&self, store: &StoreInner, index: SignatureIdx) -> DedupFuncType;
        fn get_memory(&self, store: &StoreInner, index: u32) -> Memory;
        fn get_table(&self, store: &StoreInner, index: TableIdx) -> Table;
        fn get_global(&self, store: &StoreInner, index: GlobalIdx) -> Global;
        fn get_data_segment(&self, store: &StoreInner, index: DataSegmentIdx) -> DataSegment;
        fn get_element_segment(&self, store: &StoreInner, index: ElementSegmentIdx) -> ElementSegment;
    }

    /// Returns the default memory of the current [`Instance`] for `ctx`.
    ///
    /// # Panics
    ///
    /// - If the current [`Instance`] does not belong to `ctx`.
    /// - If the current [`Instance`] does not have a linear memory.
    #[inline]
    fn get_default_memory(&self) -> Memory {
        self.get_memory(DEFAULT_MEMORY_INDEX)
    }

    /// Returns the [`Reg`] value.
    fn get_register(&self, register: Reg) -> UntypedVal {
        // Safety: - It is the responsibility of the `Executor`
        //           implementation to keep the `sp` pointer valid
        //           whenever this method is accessed.
        //         - This is done by updating the `sp` pointer whenever
        //           the heap underlying the value stack is changed.
        unsafe { self.sp.get(register) }
    }

    /// Returns the [`Reg`] value.
    fn get_register_as<T>(&self, register: Reg) -> T
    where
        T: From<UntypedVal>,
    {
        T::from(self.get_register(register))
    }

    /// Sets the [`Reg`] value to `value`.
    fn set_register(&mut self, register: Reg, value: impl Into<UntypedVal>) {
        // Safety: - It is the responsibility of the `Executor`
        //           implementation to keep the `sp` pointer valid
        //           whenever this method is accessed.
        //         - This is done by updating the `sp` pointer whenever
        //           the heap underlying the value stack is changed.
        unsafe { self.sp.set(register, value.into()) };
    }

    /// Shifts the instruction pointer to the next instruction.
    #[inline(always)]
    fn next_instr(&mut self) {
        self.next_instr_at(1)
    }

    /// Shifts the instruction pointer to the next instruction.
    ///
    /// Has a parameter `skip` to denote how many instruction words
    /// to skip to reach the next actual instruction.
    ///
    /// # Note
    ///
    /// This is used by Wasmi instructions that have a fixed
    /// encoding size of two instruction words such as [`Instruction::Branch`].
    #[inline(always)]
    fn next_instr_at(&mut self, skip: usize) {
        self.ip.add(skip)
    }

    /// Shifts the instruction pointer to the next instruction and returns `Ok(())`.
    ///
    /// # Note
    ///
    /// This is a convenience function for fallible instructions.
    #[inline(always)]
    fn try_next_instr(&mut self) -> Result<(), Error> {
        self.try_next_instr_at(1)
    }

    /// Shifts the instruction pointer to the next instruction and returns `Ok(())`.
    ///
    /// Has a parameter `skip` to denote how many instruction words
    /// to skip to reach the next actual instruction.
    ///
    /// # Note
    ///
    /// This is a convenience function for fallible instructions.
    #[inline(always)]
    fn try_next_instr_at(&mut self, skip: usize) -> Result<(), Error> {
        self.next_instr_at(skip);
        Ok(())
    }

    /// Returns the [`FrameRegisters`] of the [`CallFrame`].
    fn frame_stack_ptr_impl(value_stack: &mut ValueStack, frame: &CallFrame) -> FrameRegisters {
        // Safety: We are using the frame's own base offset as input because it is
        //         guaranteed by the Wasm validation and translation phase to be
        //         valid for all register indices used by the associated function body.
        unsafe { value_stack.stack_ptr_at(frame.base_offset()) }
    }

    /// Initializes the [`Executor`] state for the [`CallFrame`].
    ///
    /// # Note
    ///
    /// The initialization of the [`Executor`] allows for efficient execution.
    fn init_call_frame(&mut self, frame: &CallFrame) {
        Self::init_call_frame_impl(&mut self.stack.values, &mut self.sp, &mut self.ip, frame)
    }

    /// Initializes the [`Executor`] state for the [`CallFrame`].
    ///
    /// # Note
    ///
    /// The initialization of the [`Executor`] allows for efficient execution.
    fn init_call_frame_impl(
        value_stack: &mut ValueStack,
        sp: &mut FrameRegisters,
        ip: &mut InstructionPtr,
        frame: &CallFrame,
    ) {
        *sp = Self::frame_stack_ptr_impl(value_stack, frame);
        *ip = frame.instr_ptr();
    }

    /// Returns the [`Instruction::Const32`] parameter for an [`Instruction`].
    fn fetch_const32(&self, offset: usize) -> AnyConst32 {
        let mut addr: InstructionPtr = self.ip;
        addr.add(offset);
        match *addr.get() {
            Instruction::Const32(value) => value,
            _ => unreachable!("expected an Instruction::Const32 instruction word"),
        }
    }

    /// Returns the [`Instruction::Const32`] parameter for an [`Instruction`].
    fn fetch_address_offset(&self, offset: usize) -> u32 {
        u32::from(self.fetch_const32(offset))
    }

    /// Executes a generic unary [`Instruction`].
    #[inline(always)]
    fn execute_unary(&mut self, result: Reg, input: Reg, op: fn(UntypedVal) -> UntypedVal) {
        let value = self.get_register(input);
        self.set_register(result, op(value));
        self.next_instr();
    }

    /// Executes a fallible generic unary [`Instruction`].
    #[inline(always)]
    fn try_execute_unary(
        &mut self,
        result: Reg,
        input: Reg,
        op: fn(UntypedVal) -> Result<UntypedVal, TrapCode>,
    ) -> Result<(), Error> {
        let value = self.get_register(input);
        self.set_register(result, op(value)?);
        self.try_next_instr()
    }

    /// Executes a generic binary [`Instruction`].
    #[inline(always)]
    fn execute_binary(
        &mut self,
        result: Reg,
        lhs: Reg,
        rhs: Reg,
        op: fn(UntypedVal, UntypedVal) -> UntypedVal,
    ) {
        let lhs = self.get_register(lhs);
        let rhs = self.get_register(rhs);
        self.set_register(result, op(lhs, rhs));
        self.next_instr();
    }

    /// Executes a generic binary [`Instruction`].
    #[inline(always)]
    fn execute_binary_imm16<T>(
        &mut self,
        result: Reg,
        lhs: Reg,
        rhs: Const16<T>,
        op: fn(UntypedVal, UntypedVal) -> UntypedVal,
    ) where
        T: From<Const16<T>>,
        UntypedVal: From<T>,
    {
        let lhs = self.get_register(lhs);
        let rhs = UntypedVal::from(<T>::from(rhs));
        self.set_register(result, op(lhs, rhs));
        self.next_instr();
    }

    /// Executes a generic binary [`Instruction`] with reversed operands.
    #[inline(always)]
    fn execute_binary_imm16_rev<T>(
        &mut self,
        result: Reg,
        lhs: Const16<T>,
        rhs: Reg,
        op: fn(UntypedVal, UntypedVal) -> UntypedVal,
    ) where
        T: From<Const16<T>>,
        UntypedVal: From<T>,
    {
        let lhs = UntypedVal::from(<T>::from(lhs));
        let rhs = self.get_register(rhs);
        self.set_register(result, op(lhs, rhs));
        self.next_instr();
    }

    /// Executes a fallible generic binary [`Instruction`].
    #[inline(always)]
    fn try_execute_binary(
        &mut self,
        result: Reg,
        lhs: Reg,
        rhs: Reg,
        op: fn(UntypedVal, UntypedVal) -> Result<UntypedVal, TrapCode>,
    ) -> Result<(), Error> {
        let lhs = self.get_register(lhs);
        let rhs = self.get_register(rhs);
        self.set_register(result, op(lhs, rhs)?);
        self.try_next_instr()
    }

    /// Executes a fallible generic binary [`Instruction`].
    #[inline(always)]
    fn try_execute_divrem_imm16<NonZeroT>(
        &mut self,
        result: Reg,
        lhs: Reg,
        rhs: Const16<NonZeroT>,
        op: fn(UntypedVal, NonZeroT) -> Result<UntypedVal, Error>,
    ) -> Result<(), Error>
    where
        NonZeroT: From<Const16<NonZeroT>>,
    {
        let lhs = self.get_register(lhs);
        let rhs = <NonZeroT>::from(rhs);
        self.set_register(result, op(lhs, rhs)?);
        self.try_next_instr()
    }

    /// Executes a fallible generic binary [`Instruction`].
    #[inline(always)]
    fn execute_divrem_imm16<NonZeroT>(
        &mut self,
        result: Reg,
        lhs: Reg,
        rhs: Const16<NonZeroT>,
        op: fn(UntypedVal, NonZeroT) -> UntypedVal,
    ) where
        NonZeroT: From<Const16<NonZeroT>>,
    {
        let lhs = self.get_register(lhs);
        let rhs = <NonZeroT>::from(rhs);
        self.set_register(result, op(lhs, rhs));
        self.next_instr()
    }

    /// Executes a fallible generic binary [`Instruction`] with reversed operands.
    #[inline(always)]
    fn try_execute_binary_imm16_rev<T>(
        &mut self,
        result: Reg,
        lhs: Const16<T>,
        rhs: Reg,
        op: fn(UntypedVal, UntypedVal) -> Result<UntypedVal, TrapCode>,
    ) -> Result<(), Error>
    where
        T: From<Const16<T>>,
        UntypedVal: From<T>,
    {
        let lhs = UntypedVal::from(<T>::from(lhs));
        let rhs = self.get_register(rhs);
        self.set_register(result, op(lhs, rhs)?);
        self.try_next_instr()
    }

    /// Skips all [`Instruction`]s belonging to an [`Instruction::RegisterList`] encoding.
    fn skip_register_list(ip: InstructionPtr) -> InstructionPtr {
        let mut ip = ip;
        while let Instruction::RegisterList(_) = *ip.get() {
            ip.add(1);
        }
        // We skip an additional `Instruction` because we know that `Instruction::RegisterList` is always followed by one of:
        // - `Instruction::Register`
        // - `Instruction::Register2`
        // - `Instruction::Register3`.
        ip.add(1);
        ip
    }
}

impl<'engine> Executor<'engine> {
    /// Used for all [`Instruction`] words that are not meant for execution.
    ///
    /// # Note
    ///
    /// This includes [`Instruction`] variants such as [`Instruction::TableIdx`]
    /// that primarily carry parameters for actually executable [`Instruction`].
    #[inline(always)]
    fn invalid_instruction_word(&mut self) -> Result<(), Error> {
        self.execute_trap(TrapCode::UnreachableCodeReached)
    }

    /// Executes a Wasm `unreachable` instruction.
    #[inline(always)]
    fn execute_trap(&mut self, trap_code: TrapCode) -> Result<(), Error> {
        Err(Error::from(trap_code))
    }

    /// Executes an [`Instruction::ConsumeFuel`].
    #[inline(always)]
    fn execute_consume_fuel(
        &mut self,
        store: &mut StoreInner,
        block_fuel: BlockFuel,
    ) -> Result<(), Error> {
        // We do not have to check if fuel metering is enabled since
        // [`Instruction::ConsumeFuel`] are only generated if fuel metering
        // is enabled to begin with.
        store
            .fuel_mut()
            .consume_fuel_unchecked(block_fuel.to_u64())?;
        self.try_next_instr()
    }

    /// Executes an [`Instruction::RefFunc`].
    #[inline(always)]
    fn execute_ref_func(&mut self, result: Reg, func_index: FuncIdx) {
        let func = self.get_func(func_index);
        let funcref = FuncRef::new(func);
        self.set_register(result, funcref);
        self.next_instr();
    }
}

/// Extension method for [`UntypedVal`] required by the [`Executor`].
trait UntypedValueExt {
    /// Executes a fused `i32.and` + `i32.eqz` instruction.
    fn i32_and_eqz(x: UntypedVal, y: UntypedVal) -> UntypedVal;

    /// Executes a fused `i32.or` + `i32.eqz` instruction.
    fn i32_or_eqz(x: UntypedVal, y: UntypedVal) -> UntypedVal;

    /// Executes a fused `i32.xor` + `i32.eqz` instruction.
    fn i32_xor_eqz(x: UntypedVal, y: UntypedVal) -> UntypedVal;
}

impl UntypedValueExt for UntypedVal {
    fn i32_and_eqz(x: UntypedVal, y: UntypedVal) -> UntypedVal {
        (i32::from(UntypedVal::i32_and(x, y)) == 0).into()
    }

    fn i32_or_eqz(x: UntypedVal, y: UntypedVal) -> UntypedVal {
        (i32::from(UntypedVal::i32_or(x, y)) == 0).into()
    }

    fn i32_xor_eqz(x: UntypedVal, y: UntypedVal) -> UntypedVal {
        (i32::from(UntypedVal::i32_xor(x, y)) == 0).into()
    }
}
