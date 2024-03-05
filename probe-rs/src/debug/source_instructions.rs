use super::{
    canonical_path_eq,
    unit_info::{self, UnitInfo},
    ColumnType, DebugError, DebugInfo,
};
use gimli::LineSequence;
use std::{
    fmt::{Debug, Formatter},
    num::NonZeroU64,
    ops::{Range, RangeInclusive},
};
use typed_path::TypedPathBuf;

/// A verified breakpoint represents an instruction address, and the source location that it corresponds to it,
/// for locations in the target binary that comply with the DWARF standard terminology for "recommended breakpoint location".
/// This typically refers to instructions that are not part of the prologue or epilogue, and are part of the user code,
/// or are the final instruction in a sequence, before the processor begins the epilogue code.
/// The `probe-rs` debugger uses this information to identify valid halt locations for breakpoints and stepping.
#[derive(Clone, Debug)]
pub struct VerifiedBreakpoint {
    /// The address in target memory, where the breakpoint can be set.
    pub address: u64,
    /// If the breakpoint request was for a specific source location, then this field will contain the resolved source location.
    pub source_location: SourceLocation,
}

impl VerifiedBreakpoint {
    /// Return the first valid breakpoint location of the statement that is greater than OR equal to `address`.
    /// e.g., if the `address` is the current program counter, then the return value will be the next valid halt address
    /// in the current sequence.
    pub(crate) fn for_address(
        debug_info: &DebugInfo,
        address: u64,
    ) -> Result<VerifiedBreakpoint, DebugError> {
        let sequence = Sequence::from_address(debug_info, address)?;

        // Cycle through various degrees of matching, to find the most relevant source location.
        if let Some(verified_breakpoint) = match_address(&sequence, address, debug_info) {
            tracing::debug!(
                "Found valid breakpoint for address: {:#010x} : {verified_breakpoint:?}",
                &address
            );
            return Ok(verified_breakpoint);
        }
        // If we get here, we have not found a valid breakpoint location.
        let message = format!("Could not identify a valid breakpoint for address: {address:#010x}. Please consider using instruction level stepping.");
        Err(DebugError::WarnAndContinue { message })
    }

    /// Identifying the breakpoint location for a specific location (path, line, colunmn) is a bit more complex,
    /// compared to the `for_address()` method, due to a few factors:
    /// - The correct program instructions, may be in any of the compilation units of the current program.
    /// - The debug information may not contain data for the "specific source" location requested:
    ///   - DWARFv5 standard, section 6.2, allows omissions based on certain conditions. In this case,
    ///    we need to find the closest "relevant" source location that has valid debug information.
    ///   - The requested location may not be a valid source location, e.g. when the
    ///    debug information has been optimized away. In this case we will return an appropriate error.
    /// #### The logic used to find the "most relevant" source location is as follows:
    /// 1. Filter  [`UnitInfo`] , by using [`LineProgramHeader`] to match units that include the requested path.
    /// 2. For each matching compilation unit, get the [`LineProgram`] and [`Vec<LineSequence>`].
    /// 3. Filter the [`Vec<LineSequence>`] entries to only include sequences that match the requested path.
    /// 3. Convert remaining [`LineSequence`], to [`Sequence`].
    /// 4. Return the first [`Sequence`] that contains the requested source location.
    ///   4a. This may be an exact match on file/line/column, or,
    ///   4b. Failing an exact match, a match on file/line only.
    ///   4c. Failing that, a match on file only, where the line number is the "next" available instruction,
    ///       on the next available line of the specified file.
    #[allow(dead_code)] // temporary, while this PR is a WIP
    pub(crate) fn for_source_location(
        debug_info: &DebugInfo,
        path: &TypedPathBuf,
        line: u64,
        column: Option<u64>,
    ) -> Result<Self, DebugError> {
        for program_unit in debug_info.unit_infos.as_slice() {
            let Some(ref line_program) = program_unit.unit.line_program else {
                // Not all compilation units need to have debug line information, so we skip those.
                continue;
            };
            // Keep track of the matching file index to avoid having to lookup and match the full path
            // for every row in the program line sequence.
            let mut matching_file_index = None;
            if line_program
                .header()
                .file_names()
                .iter()
                .enumerate()
                .any(|(file_index, _)| {
                    debug_info
                        .get_path(&program_unit.unit, file_index as u64)
                        .map(|combined_path: TypedPathBuf| {
                            if canonical_path_eq(path, &combined_path) {
                                matching_file_index = Some(file_index as u64);
                                true
                            } else {
                                false
                            }
                        })
                        .unwrap_or(false)
                })
            {
                let Ok((complete_line_program, line_sequences)) = line_program.clone().sequences()
                else {
                    continue;
                };
                for line_sequence in line_sequences {
                    let sequence = Sequence::from_line_sequence(
                        debug_info,
                        program_unit,
                        complete_line_program.clone(),
                        &line_sequence,
                    );

                    if let Some(verified_breakpoint) = match_location(
                        &sequence,
                        matching_file_index,
                        line,
                        column,
                        debug_info,
                        program_unit,
                    ) {
                        return Ok(verified_breakpoint);
                    }
                }
            }
        }
        // If we get here, we have not found a valid breakpoint location.
        Err(DebugError::Other(anyhow::anyhow!("No valid breakpoint information found for file: {}, line: {line:?}, column: {column:?}", path.to_path().display())))
    }
}

/// Find the valid halt instruction location that is equal to, or greater than, the address.
fn match_address(
    sequence: &Sequence<'_>,
    address: u64,
    debug_info: &DebugInfo,
) -> Option<VerifiedBreakpoint> {
    println!("Looking for halt instruction at address={address:#010x}\n{sequence:?}");

    if sequence.address_range.contains(&address) {
        sequence
            .blocks
            .iter()
            .find_map(|block| block.match_address(address))
            .and_then(|instruction| {
                SourceLocation::from_instruction(debug_info, sequence.program_unit, instruction)
                    .map(|source_location| VerifiedBreakpoint {
                        address: instruction.address,
                        source_location,
                    })
            })
    } else {
        None
    }
}

/// Find the valid halt instruction location that matches the file, line and column.
fn match_location(
    sequence: &Sequence<'_>,
    matching_file_index: Option<u64>,
    line: u64,
    column: Option<u64>,
    debug_info: &DebugInfo,
    program_unit: &UnitInfo,
) -> Option<VerifiedBreakpoint> {
    println!(
        "Looking for halt instruction on line={line:04}  col={:05}  f={:02} - {}\n{sequence:?}",
        column.unwrap(),
        matching_file_index.unwrap(),
        debug_info
            .get_path(&program_unit.unit, matching_file_index.unwrap())
            .unwrap()
            .to_string_lossy()
    );

    sequence
        .blocks
        .iter()
        .find_map(|block| block.match_location(matching_file_index, line, column))
        .and_then(|instruction| {
            SourceLocation::from_instruction(debug_info, sequence.program_unit, instruction).map(
                |source_location| VerifiedBreakpoint {
                    address: instruction.address,
                    source_location,
                },
            )
        })
}

fn serialize_typed_path<S>(path: &Option<TypedPathBuf>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match path {
        Some(path) => serializer.serialize_str(&path.to_string_lossy()),
        None => serializer.serialize_none(),
    }
}

/// A specific location in source code.
/// Each unique line, column, file and directory combination is a unique source location.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SourceLocation {
    /// The line number in the source file with zero based indexing.
    pub line: Option<u64>,
    /// The column number in the source file with zero based indexing.
    pub column: Option<ColumnType>,
    /// The file name of the source file.
    pub file: Option<String>,
    /// The directory of the source file.
    #[serde(serialize_with = "serialize_typed_path")]
    pub directory: Option<TypedPathBuf>,
}

impl SourceLocation {
    /// Resolve debug information for a [`Instruction`] and create a [`SourceLocation`].
    fn from_instruction(
        debug_info: &DebugInfo,
        program_unit: &unit_info::UnitInfo,
        instruction: &Instruction,
    ) -> Option<SourceLocation> {
        debug_info
            .find_file_and_directory(&program_unit.unit, instruction.file_index)
            .map(|(file, directory)| SourceLocation {
                line: instruction.line.map(std::num::NonZeroU64::get),
                column: Some(instruction.column),
                file,
                directory,
            })
    }

    /// Get the full path of the source file
    pub fn combined_typed_path(&self) -> Option<TypedPathBuf> {
        let combined_path = self
            .directory
            .as_ref()
            .and_then(|dir| self.file.as_ref().map(|file| dir.join(file)));

        combined_path
    }
}

/// Keep track of all the instruction locations required to satisfy the operations of [`SteppingMode`].
/// This is a list of target instructions, belonging to a [`gimli::LineSequence`],
/// and filters it to only user code instructions (no prologue code, and no non-statement instructions),
/// so that we are left only with what DWARF terms as "recommended breakpoint location".
struct Sequence<'debug_info> {
    /// The `address_range.start` is the starting address of the program counter for which this sequence is valid,
    /// and allows us to identify target instruction locations where the program counter lies inside the prologue.
    /// The `address_range.end` is the first address that is not covered by this sequence within the line number program,
    /// and allows us to identify when stepping over a instruction location would result in leaving a sequence.
    /// - This is typically the instruction address of the first instruction in the next sequence,
    ///   which may also be the first instruction in a new function.
    address_range: Range<u64>,
    /// See [`Block`].
    blocks: Vec<Block>,
    // The following private fields are required to resolve the source location information for
    // each instruction location.
    debug_info: &'debug_info DebugInfo,
    program_unit: &'debug_info UnitInfo,
}

impl Debug for Sequence<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "Sequence range: {:#010x}..{:#010x}",
            self.address_range.start, self.address_range.end
        )?;
        for block in &self.blocks {
            writeln!(
                f,
                "  Block range: {:#010x}..={:#010x}",
                block.included_addresses.start(),
                block.included_addresses.end()
            )?;
            for instruction in &block.instructions {
                writeln!(
                    f,
                    "    {instruction:?} - {:?}",
                    self.debug_info
                        .get_path(&self.program_unit.unit, instruction.file_index)
                        .map(
                            |file_path| TypedPathBuf::from_unix(file_path.file_name().unwrap())
                                .to_string_lossy()
                                .to_string()
                        )
                        .unwrap_or("<unknown file>".to_string())
                )?;
            }
            writeln!(f)?;
        }
        Ok(())
    }
}

impl<'debug_info> Sequence<'debug_info> {
    /// Extract all the instruction locations, belonging to the active sequence (i.e. the sequence that contains the `address`).
    fn from_address(
        debug_info: &'debug_info DebugInfo,
        program_counter: u64,
    ) -> Result<Self, DebugError> {
        let program_unit = debug_info.compile_unit_info(program_counter)?;
        let (offset, address_size) = if let Some(line_program) =
            program_unit.unit.line_program.clone()
        {
            (
                line_program.header().offset(),
                line_program.header().address_size(),
            )
        } else {
            let message = "The specified source location does not have any line_program information available. Please consider using instruction level stepping.".to_string();
            return Err(DebugError::WarnAndContinue { message });
        };

        // Get the sequences of rows from the CompleteLineProgram at the given program_counter.
        let incomplete_line_program =
            debug_info
                .debug_line_section
                .program(offset, address_size, None, None)?;
        let (complete_line_program, line_sequences) = incomplete_line_program.sequences()?;

        // Get the sequence of rows that belongs to the program_counter.
        let Some(line_sequence) = line_sequences.iter().find(|line_sequence| {
            line_sequence.start <= program_counter && program_counter < line_sequence.end
        }) else {
            let message = "The specified source location does not have any line information available. Please consider using instruction level stepping.".to_string();
            return Err(DebugError::WarnAndContinue { message });
        };
        let sequence = Self::from_line_sequence(
            debug_info,
            program_unit,
            complete_line_program,
            line_sequence,
        );

        if sequence.len() == 0 {
            let message = "Could not find valid instruction locations for this address. Consider using instruction level stepping.".to_string();
            Err(DebugError::WarnAndContinue { message })
        } else {
            tracing::trace!(
                "Instruction location for pc={:#010x}\n{:?}",
                program_counter,
                sequence
            );
            Ok(sequence)
        }
    }

    /// Build [`Sequence`] from a [`gimli::LineSequence`], with all the markers we need to determine valid halt locations.
    fn from_line_sequence(
        debug_info: &'debug_info DebugInfo,
        program_unit: &'debug_info UnitInfo,
        complete_line_program: gimli::CompleteLineProgram<
            gimli::EndianReader<gimli::LittleEndian, std::rc::Rc<[u8]>>,
            usize,
        >,
        line_sequence: &LineSequence<gimli::EndianReader<gimli::LittleEndian, std::rc::Rc<[u8]>>>,
    ) -> Self {
        let program_language = program_unit.get_language();
        let mut sequence_rows = complete_line_program.resume_from(line_sequence);

        // We have enough information to create the Sequence.
        let mut sequence = Sequence {
            address_range: line_sequence.start..line_sequence.end,
            blocks: Vec::new(),
            debug_info,
            program_unit,
        };

        // HACK: Temporary code to add all known instructions to a single block.
        let mut block = Block {
            included_addresses: line_sequence.start..=line_sequence.start,
            instructions: Vec::new(),
        };

        let mut prologue_completed = false;
        let mut previous_row: Option<gimli::LineRow> = None;
        while let Ok(Some((_, row))) = sequence_rows.next_row() {
            if !prologue_completed && is_prologue_complete(row, program_language, previous_row) {
                // This is the first row after the prologue, so we close off the previous block, ...
                sequence.blocks.push(block);
                // ... and start a new block.
                block = Block {
                    included_addresses: row.address()..=row.address(),
                    instructions: Vec::new(),
                };
                prologue_completed = true;
            }

            if !prologue_completed {
                log_row_eval(line_sequence, row, "  inside prologue>");
            } else {
                log_row_eval(line_sequence, row, "  after prologue>");
            }

            // The end of the sequence is not a valid halt location,
            // nor is it a valid instruction in the current sequence.
            if row.end_sequence() {
                break;
            }

            block.add(prologue_completed, row, previous_row.as_ref());
            previous_row = Some(*row);
        }
        // Add the last block to the sequence.
        if !block.instructions.is_empty() {
            sequence.blocks.push(block);
        }
        sequence
    }

    /// Get the number of instruction locations in the list.
    fn len(&self) -> usize {
        self.blocks.len()
    }
}

/// Test if the current row signals that we are beyond the prologue, and into user code
fn is_prologue_complete(
    row: &gimli::LineRow,
    program_language: gimli::DwLang,
    previous_row: Option<gimli::LineRow>,
) -> bool {
    let mut prologue_completed = row.prologue_end();

    // For GNU C, it is known that the `DW_LNS_set_prologue_end` is not set, so we employ the same heuristic as GDB to determine when the prologue is complete.
    // For other C compilers in the C99/11/17 standard, they will either set the `DW_LNS_set_prologue_end` or they will trigger this heuristic also.
    // See https://gcc.gnu.org/legacy-ml/gcc-patches/2011-03/msg02106.html
    if !prologue_completed
        && matches!(
            program_language,
            gimli::DW_LANG_C99 | gimli::DW_LANG_C11 | gimli::DW_LANG_C17
        )
    {
        if let Some(prev_row) = previous_row {
            if row.end_sequence()
                || (row.is_stmt()
                    && (row.file_index() == prev_row.file_index()
                        && (row.line() != prev_row.line() || row.line().is_none())))
            {
                prologue_completed = true;
            }
        }
    }
    prologue_completed
}

/// The concept of an instruction block is based on
/// [Rust's MIR basic block definition](https://rustc-dev-guide.rust-lang.org/appendix/background.html#cfg)
/// The concept is also a close match for how the DAP specification defines the a `statement`
/// [SteppingGranularity](https://microsoft.github.io/debug-adapter-protocol/specification#Types_SteppingGranularity)
/// In the context of the `probe-rs` debugger, an instruction block is a contiguous series of instructions
/// which belong to a single [`Sequence`].
/// The key difference between instructions in a block, and those in a [`gimli::LineSequence`], is that we can rely
/// on the 'next' instruction in the block to be the 'next' instruction the processor will execute (barring any interrupts).
/// ### Implementation discussion:
/// Indentifying the boundaries of each [`Block`] is the key to identifying valid halt locations, and is the primary
/// purpose of the [`Block`] struct. Current versions of Rust (up to rustc 1.76.0) does not populate the
/// `DW_LNS_basic_block` attribute of the line program rows in the DWARF debug information. The implication of this is that
/// we need to infer the boundaries of each block withing the sequence of instructions, from other blocks, as well as
/// from the prologue and epilogue markers. The approach taken is as follows:
/// - The first block is the prologue block, and is identified by the `DW_LNS_set_prologue_end` attribute.
/// - If the sequence starting address is a non-inlined function, then if the DWARF `DW_AT_subprogram` attribute
///   for the function uses:
///   - `DW_AT_ranges`, we use those ranges as initial block boundaries. These ranges only covers
///      parts of the sequence, and we start by creating a block for each covered range, and blocks
///      for the remaining covered ranges.
struct Block {
    /// The range of addresses that the block covers is 'inclusive' on both ends.
    included_addresses: RangeInclusive<u64>,
    instructions: Vec<Instruction>,
}

impl Block {
    /// Find the valid halt instruction location that is equal to, or greater than, the address.
    fn match_address(&self, address: u64) -> Option<&Instruction> {
        if self.included_addresses.contains(&address) {
            self.instructions.iter().find(|&location| {
                location.instruction_type == InstructionType::HaltLocation
                    && location.address >= address
            })
        } else {
            None
        }
    }

    /// Find the valid halt instruction location that that matches the `file`, `line` and `column`.
    /// If `column` is `None`, then the first instruction location that matches the `file` and `line` is returned.
    /// TODO: If there is a match, but it is not a valid halt location, then the next valid halt location is returned.
    fn match_location(
        &self,
        matching_file_index: Option<u64>,
        line: u64,
        column: Option<u64>,
    ) -> Option<&Instruction> {
        // Cycle through various degrees of matching, to find the most relevant source location.
        if let Some(supplied_column) = column {
            // Try an exact match.
            self.instructions
                .iter()
                .find(|&location| {
                    location.instruction_type == InstructionType::HaltLocation
                        && matching_file_index == Some(location.file_index)
                        && NonZeroU64::new(line) == location.line
                        && ColumnType::from(supplied_column) == location.column
                })
                .or_else(|| {
                    // Try without a column specifier.
                    self.instructions.iter().find(|&location| {
                        location.instruction_type == InstructionType::HaltLocation
                            && matching_file_index == Some(location.file_index)
                            && NonZeroU64::new(line) == location.line
                    })
                })
        } else {
            self.instructions.iter().find(|&location| {
                location.instruction_type == InstructionType::HaltLocation
                    && matching_file_index == Some(location.file_index)
                    && NonZeroU64::new(line) == location.line
            })
        }
    }

    /// Add a instruction locations to the list.
    fn add(
        &mut self,
        prologue_completed: bool,
        row: &gimli::LineRow,
        previous_row: Option<&gimli::LineRow>,
    ) {
        // Workaround the line number issue (if recorded as 0 in the DWARF, then gimli reports it as None).
        // For debug purposes, it makes more sense to be the same as the previous line, which almost always
        // has the same file index and column value.
        // This prevents the debugger from jumping to the top of the file unexpectedly.
        let mut instruction_line = row.line();
        if let Some(prev_row) = previous_row {
            if row.line().is_none()
                && prev_row.line().is_some()
                && row.file_index() == prev_row.file_index()
                && prev_row.column() == row.column()
            {
                instruction_line = prev_row.line();
            }
        }

        let instruction = Instruction {
            address: row.address(),
            file_index: row.file_index(),
            line: instruction_line,
            column: row.column().into(),
            instruction_type: if !prologue_completed {
                InstructionType::Prologue
            } else if row.epilogue_begin() || row.is_stmt() {
                InstructionType::HaltLocation
            } else {
                InstructionType::Unspecified
            },
        };
        self.included_addresses = *self.included_addresses.start()..=row.address();
        self.instructions.push(instruction);
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
/// The type of instruction, as defined by [`gimli::LineRow`] attributes and relative position in the sequence.
enum InstructionType {
    /// We need to keep track of source lines that signal function signatures,
    /// even if their program lines are not valid halt locations.
    Prologue,
    /// DWARF defined "recommended breakpoint location",
    /// typically marked with `is_stmt` or `epilogue_begin`.
    HaltLocation,
    /// Any other instruction that is not part of the prologue or epilogue, and is not a statement,
    /// is considered to be an unspecified instruction type.
    Unspecified,
}

#[derive(Copy, Clone)]
/// - A [`Instruction`] filters and maps [`gimli::LineRow`] entries to be used for determining valid halt points.
///   - Each [`Instruction`] maps to a single machine instruction on target.
///   - For establishing valid halt locations (breakpoint or stepping), we are only interested,
///     in the [`Instruction`]'s that represent DWARF defined `statements`,
///     which are not part of the prologue or epilogue.
/// - A line of code in a source file may contain multiple instruction locations, in which case
///     a new [`Instruction`] with unique `column` is created.
/// - A [`Sequence`] is a series of contiguous [`Instruction`]'s.
struct Instruction {
    address: u64,
    file_index: u64,
    line: Option<NonZeroU64>,
    column: ColumnType,
    instruction_type: InstructionType,
}

impl Instruction {}

impl Debug for Instruction {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:010x}, line={:04}  col={:05}  f={:02}, type={:?}",
            &self.address,
            match &self.line {
                Some(line) => line.get(),
                None => 0,
            },
            match &self.column {
                ColumnType::LeftEdge => 0,
                ColumnType::Column(column) => column.to_owned(),
            },
            &self.file_index,
            &self.instruction_type,
        )?;
        Ok(())
    }
}

/// Helper function to avoid code duplication when logging of information during row evaluation.
fn log_row_eval(
    active_sequence: &LineSequence<super::GimliReader>,
    row: &gimli::LineRow,
    status: &str,
) {
    tracing::trace!("Sequence: line={:04} col={:05} f={:02} stmt={:5} ep={:5} es={:5} eb={:5} : {:#010X}<={:#010X}<{:#010X} : {}",
        match row.line() {
            Some(line) => line.get(),
            None => 0,
        },
        match row.column() {
            gimli::ColumnType::LeftEdge => 0,
            gimli::ColumnType::Column(column) => column.get(),
        },
        row.file_index(),
        row.is_stmt(),
        row.prologue_end(),
        row.end_sequence(),
        row.epilogue_begin(),
        active_sequence.start,
        row.address(),
        active_sequence.end,
        status);
}
