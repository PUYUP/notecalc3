#![feature(ptr_offset_from, const_if_match, const_fn, const_panic, drain_filter)]
#![feature(const_generics)]
#![feature(type_alias_impl_trait)]

use crate::calc::{add_op, evaluate_tokens, CalcResult, EvaluationResult};
use crate::consts::{LINE_NUM_CONSTS, STATIC_LINE_IDS};
use crate::editor::editor::{Editor, EditorInputEvent, InputModifiers, Pos, Selection};
use crate::editor::editor_content::EditorContent;
use crate::matrix::MatrixData;
use crate::renderer::{render_result, render_result_into};
use crate::shunting_yard::ShuntingYard;
use crate::token_parser::{OperatorTokenType, Token, TokenParser, TokenType};
use crate::units::consts::{create_prefixes, init_units};
use crate::units::units::Units;
use crate::units::UnitPrefixes;
use bigdecimal::BigDecimal;
use bigdecimal::Zero;
use smallvec::SmallVec;
use std::io::Cursor;
use std::mem::MaybeUninit;
use std::ops::Range;

mod calc;
mod matrix;
mod shunting_yard;
mod token_parser;
mod units;

pub mod consts;
pub mod editor;
pub mod renderer;

const MAX_EDITOR_WIDTH: usize = 120;
const LEFT_GUTTER_WIDTH: usize = 1 + 2 + 1;
const MAX_LINE_COUNT: usize = 64;

pub enum Click {
    Simple(Pos),
    Drag(Pos),
}

impl Click {
    fn with_pos(&self, pos: Pos) -> Click {
        if matches!(self, Click::Simple(_)) {
            Click::Simple(pos)
        } else {
            Click::Drag(pos)
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub enum TextStyle {
    Normal,
    Bold,
    Underline,
    Italy,
}

#[derive(Debug)]
pub struct RenderUtf8TextMsg<'a> {
    pub text: &'a [char],
    pub row: usize,
    pub column: usize,
}

#[derive(Debug)]
pub struct RenderAsciiTextMsg<'a> {
    pub text: &'a [u8],
    pub row: usize,
    pub column: usize,
}

#[derive(Debug)]
pub struct RenderStringMsg {
    pub text: String,
    pub row: usize,
    pub column: usize,
}

#[repr(C)]
#[derive(Debug)]
pub enum OutputMessage<'a> {
    SetStyle(TextStyle),
    SetColor(u32),
    RenderChar(usize, usize, char),
    RenderUtf8Text(RenderUtf8TextMsg<'a>),
    RenderAsciiText(RenderAsciiTextMsg<'a>),
    RenderString(RenderStringMsg),
    RenderRectangle {
        x: usize,
        y: usize,
        w: usize,
        h: usize,
    },
}

#[derive(Debug)]
pub struct RenderBuckets<'a> {
    pub ascii_texts: Vec<RenderAsciiTextMsg<'a>>,
    pub utf8_texts: Vec<RenderUtf8TextMsg<'a>>,
    pub numbers: Vec<RenderUtf8TextMsg<'a>>,
    pub units: Vec<RenderUtf8TextMsg<'a>>,
    pub operators: Vec<RenderUtf8TextMsg<'a>>,
    pub variable: Vec<RenderUtf8TextMsg<'a>>,
    pub line_ref_results: Vec<RenderStringMsg>,
    pub custom_commands: [Vec<OutputMessage<'a>>; 2],
}

#[repr(C)]
pub enum Layer {
    BehindText,
    AboveText,
}

impl<'a> RenderBuckets<'a> {
    pub fn new() -> RenderBuckets<'a> {
        RenderBuckets {
            ascii_texts: Vec::with_capacity(128),
            utf8_texts: Vec::with_capacity(128),
            custom_commands: [Vec::with_capacity(128), Vec::with_capacity(128)],
            numbers: Vec::with_capacity(32),
            units: Vec::with_capacity(32),
            operators: Vec::with_capacity(32),
            variable: Vec::with_capacity(32),
            line_ref_results: Vec::with_capacity(32),
        }
    }

    pub fn set_color(&mut self, layer: Layer, color: u32) {
        self.custom_commands[layer as usize].push(OutputMessage::SetColor(color));
    }

    pub fn draw_rect(&mut self, layer: Layer, x: usize, y: usize, w: usize, h: usize) {
        self.custom_commands[layer as usize].push(OutputMessage::RenderRectangle { x, y, w, h });
    }

    pub fn draw_char(&mut self, layer: Layer, x: usize, y: usize, ch: char) {
        self.custom_commands[layer as usize].push(OutputMessage::RenderChar(x, y, ch));
    }

    pub fn draw_text(&mut self, layer: Layer, x: usize, y: usize, text: &'static [char]) {
        self.custom_commands[layer as usize].push(OutputMessage::RenderUtf8Text(
            RenderUtf8TextMsg {
                text,
                row: y,
                column: x,
            },
        ));
    }

    pub fn draw_ascii_text(&mut self, layer: Layer, x: usize, y: usize, text: &'static [u8]) {
        self.custom_commands[layer as usize].push(OutputMessage::RenderAsciiText(
            RenderAsciiTextMsg {
                text,
                row: y,
                column: x,
            },
        ));
    }

    pub fn draw_string(&mut self, layer: Layer, x: usize, y: usize, text: String) {
        self.custom_commands[layer as usize].push(OutputMessage::RenderString(RenderStringMsg {
            text: text.clone(),
            row: y,
            column: x,
        }));
    }
}

#[derive(Eq, PartialEq, Copy, Clone)]
pub enum ResultFormat {
    Bin,
    Dec,
    Hex,
}

#[derive(Clone)]
pub struct LineData {
    line_id: usize,
    result_format: ResultFormat,
}

impl Default for LineData {
    fn default() -> Self {
        LineData {
            line_id: 0,
            result_format: ResultFormat::Dec,
        }
    }
}

pub struct MatrixEditing {
    editor_content: EditorContent<usize>,
    editor: Editor,
    row_count: usize,
    col_count: usize,
    current_cell: Pos,
    start_text_index: usize,
    end_text_index: usize,
    row_index: usize,
    cell_strings: Vec<String>,
}

impl MatrixEditing {
    const TMP_BUF_LEN_PER_CELL: usize = 32;
    pub fn new<'a>(
        row_count: usize,
        col_count: usize,
        src_canvas: &[char],
        row_index: usize,
        start_text_index: usize,
        end_text_index: usize,
        step_in_pos: Pos,
    ) -> MatrixEditing {
        let current_cell = if step_in_pos.row == row_index {
            if step_in_pos.column > start_text_index {
                // from right
                Pos::from_row_column(0, col_count - 1)
            } else {
                // from left
                Pos::from_row_column(0, 0)
            }
        } else if step_in_pos.row < row_index {
            // from above
            Pos::from_row_column(0, 0)
        } else {
            // from below
            Pos::from_row_column(row_count - 1, 0)
        };

        let mut editor_content = EditorContent::new(32);
        let mut mat_edit = MatrixEditing {
            row_index,
            start_text_index,
            end_text_index,
            editor: Editor::new(&mut editor_content),
            editor_content,
            row_count,
            col_count,
            current_cell,
            cell_strings: Vec::with_capacity(row_count * col_count),
        };
        let mut str: String = String::with_capacity(8);
        let mut row_i = 0;
        let mut col_i = 0;
        let mut can_ignore_ws = true;
        for ch in src_canvas {
            match ch {
                '[' => {
                    //ignore
                }
                ']' => {
                    break;
                }
                ',' => {
                    col_i += 1;
                    mat_edit.cell_strings.push(str);
                    str = String::with_capacity(8);
                    str = String::with_capacity(8);
                    can_ignore_ws = true;
                }
                ';' => {
                    row_i += 1;
                    col_i = 0;
                    mat_edit.cell_strings.push(str);
                    str = String::with_capacity(8);
                    can_ignore_ws = true;
                }
                _ if ch.is_ascii_whitespace() && can_ignore_ws => {
                    // ignore
                }
                _ => {
                    can_ignore_ws = false;
                    str.push(*ch);
                }
            }
        }
        if str.len() > 0 {
            mat_edit.cell_strings.push(str);
        }

        let cell_index = mat_edit.current_cell.row * col_count + mat_edit.current_cell.column;
        mat_edit
            .editor_content
            .set_content(&mat_edit.cell_strings[cell_index]);
        if step_in_pos.row == row_index && step_in_pos.column > start_text_index {
            // from right
            mat_edit
                .editor
                .set_cursor_pos_r_c(0, mat_edit.editor_content.line_len(0));
        } else {
            mat_edit.editor.set_cursor_pos_r_c(0, 0);
        }

        mat_edit
    }

    fn change_cell(&mut self, new_pos: Pos) {
        self.save_editor_content();

        let new_content = &self.cell_strings[new_pos.row * self.col_count + new_pos.column];
        self.editor_content.set_content(new_content);

        self.current_cell = new_pos;
    }

    fn save_editor_content(&mut self) {
        let mut backend = &mut self.cell_strings
            [self.current_cell.row * self.col_count + self.current_cell.column];
        backend.clear();
        self.editor_content.write_content_into(&mut backend);
    }

    fn render<'b>(
        &self,
        mut render_x: usize,
        mut render_y: usize,
        current_editor_width: usize,
        render_buckets: &mut RenderBuckets<'b>,
        rendered_row_height: usize,
    ) -> usize {
        let vert_align_offset = (rendered_row_height - self.row_count) / 2;

        render_buckets.operators.push(RenderUtf8TextMsg {
            text: &['⎡'],
            row: render_y + vert_align_offset,
            column: render_x + LEFT_GUTTER_WIDTH,
        });
        for i in 1..self.row_count - 1 {
            render_buckets.operators.push(RenderUtf8TextMsg {
                text: &['⎢'],
                row: render_y + i + vert_align_offset,
                column: render_x + LEFT_GUTTER_WIDTH,
            });
        }
        render_buckets.operators.push(RenderUtf8TextMsg {
            text: &['⎣'],
            row: render_y + self.row_count - 1 + vert_align_offset,
            column: render_x + LEFT_GUTTER_WIDTH,
        });
        render_x += 1;

        for col_i in 0..self.col_count {
            if render_x >= current_editor_width {
                return render_x;
            }
            let max_width: usize = (0..self.row_count)
                .map(|row_i| {
                    if self.current_cell == Pos::from_row_column(row_i, col_i) {
                        self.editor_content.line_len(0)
                    } else {
                        self.cell_strings[row_i * self.col_count + col_i].len()
                    }
                })
                .max()
                .unwrap();
            for row_i in 0..self.row_count {
                let len: usize = if self.current_cell == Pos::from_row_column(row_i, col_i) {
                    self.editor_content.line_len(0)
                } else {
                    self.cell_strings[row_i * self.col_count + col_i].len()
                };
                let padding_x = max_width - len;
                let text_len = len.min(
                    (current_editor_width as isize - (render_x + padding_x) as isize).max(0)
                        as usize,
                );

                if self.current_cell == Pos::from_row_column(row_i, col_i) {
                    render_buckets.set_color(Layer::AboveText, 0xBBBBBB_55);
                    render_buckets.draw_rect(
                        Layer::AboveText,
                        render_x + padding_x + LEFT_GUTTER_WIDTH,
                        render_y + row_i + vert_align_offset,
                        text_len,
                        1,
                    );
                    let chars = &self.editor_content.lines().next().unwrap();
                    render_buckets.set_color(Layer::AboveText, 0x000000_FF);
                    for (i, char) in chars.iter().enumerate() {
                        render_buckets.draw_char(
                            Layer::AboveText,
                            render_x + padding_x + LEFT_GUTTER_WIDTH + i,
                            render_y + row_i + vert_align_offset,
                            *char,
                        );
                    }
                } else {
                    let chars = &self.cell_strings[row_i * self.col_count + col_i];
                    render_buckets.set_color(Layer::AboveText, 0x000000_FF);
                    render_buckets.draw_string(
                        Layer::AboveText,
                        render_x + padding_x + LEFT_GUTTER_WIDTH,
                        render_y + row_i + vert_align_offset,
                        (&chars[0..text_len]).to_owned(),
                    );
                }

                if self.current_cell == Pos::from_row_column(row_i, col_i) {
                    if self.editor.is_cursor_shown() {
                        render_buckets.set_color(Layer::AboveText, 0x000000_FF);
                        render_buckets.draw_char(
                            Layer::AboveText,
                            (self.editor.get_selection().get_cursor_pos().column
                                + LEFT_GUTTER_WIDTH)
                                + render_x
                                + padding_x,
                            render_y + row_i + vert_align_offset,
                            '▏',
                        );
                    }
                }
            }
            render_x += if col_i + 1 < self.col_count {
                max_width + 2
            } else {
                max_width
            };
        }

        render_buckets.operators.push(RenderUtf8TextMsg {
            text: &['⎤'],
            row: render_y + vert_align_offset,
            column: render_x + LEFT_GUTTER_WIDTH,
        });
        for i in 1..self.row_count - 1 {
            render_buckets.operators.push(RenderUtf8TextMsg {
                text: &['⎥'],
                row: render_y + i + vert_align_offset,
                column: render_x + LEFT_GUTTER_WIDTH,
            });
        }
        render_buckets.operators.push(RenderUtf8TextMsg {
            text: &['⎦'],
            row: render_y + self.row_count - 1 + vert_align_offset,
            column: render_x + LEFT_GUTTER_WIDTH,
        });
        render_x += 1;

        render_x
    }
}

#[derive(Eq, PartialEq)]
enum EditorObjectType {
    Matrix { row_count: usize, col_count: usize },
    LineReference,
}
struct EditorObject {
    typ: EditorObjectType,
    row: usize,
    start_x: usize,
    end_x: usize,
}

pub struct NoteCalcApp<'a> {
    client_width: usize,
    units: Units<'a>,
    pub editor: Editor,
    pub editor_content: EditorContent<LineData>,
    prev_cursor_pos: Pos,
    prefixes: &'static UnitPrefixes,
    result_buffer: [u8; 1024],
    matrix_editing: Option<MatrixEditing>,
    editor_click: Option<Click>,
    editor_objects: Vec<EditorObject>,
    line_reference_chooser: Option<usize>,
    line_id_generator: usize,
    has_result_bitset: u64,
    result_gutter_x: usize,
    right_gutter_is_dragged: bool,
}

struct RenderPass {
    current_editor_width: usize,
    current_result_width: usize,
    editor_pos: Pos,
    render_pos: Pos,
    // contains the y position for each editor line
    editor_y_to_render_y: [usize; MAX_LINE_COUNT],
    editor_y_to_vert_align: [usize; MAX_LINE_COUNT],
    clicked_editor_pos: Option<Click>,
    longest_row_len: usize,
    rendered_row_height: usize,
    vert_align_offset: usize,
    cursor_render_x_offset: isize,
    result_gutter_x: usize,
    prev_matrix_format: Option<ResultLengths>,
}

impl RenderPass {
    const RIGHT_GUTTER_WIDTH: usize = 3;
    const MIN_RESULT_PANEL_WIDTH: usize = 30;

    fn new(client_width: usize, result_gutter_x: usize) -> RenderPass {
        let mut r = RenderPass {
            current_editor_width: 0,
            current_result_width: 0,
            editor_pos: Default::default(),
            render_pos: Default::default(),
            editor_y_to_render_y: [0; MAX_LINE_COUNT],
            editor_y_to_vert_align: [0; MAX_LINE_COUNT],
            clicked_editor_pos: None,
            longest_row_len: 0,
            rendered_row_height: 0,
            vert_align_offset: 0,
            cursor_render_x_offset: 0,
            prev_matrix_format: None,
            result_gutter_x,
        };

        r.current_editor_width = result_gutter_x - LEFT_GUTTER_WIDTH;
        r.current_result_width = client_width - result_gutter_x - RenderPass::RIGHT_GUTTER_WIDTH;
        r
    }
    pub fn new_line_started(&mut self) {
        let rendered_len = self.render_pos.column;
        if rendered_len > self.longest_row_len {
            self.longest_row_len = rendered_len;
        }
        self.editor_pos.column = 0;

        self.render_pos.column = 0;
        self.editor_y_to_render_y[self.editor_pos.row] = self.render_pos.row;

        self.cursor_render_x_offset = 0;
    }

    fn line_render_ended(&mut self) {
        self.editor_pos.row += 1;
        self.render_pos.row += self.rendered_row_height;
    }

    fn set_fix_row_height(&mut self, height: usize) {
        self.rendered_row_height = height;
        self.vert_align_offset = 0;
        self.editor_y_to_vert_align[self.editor_pos.row] = self.vert_align_offset;
    }

    fn calc_rendered_row_height(&mut self, tokens: &[Token], result_row_height: usize) {
        let mut max_height = 1;
        for token in tokens {
            match token.typ {
                TokenType::Operator(OperatorTokenType::Matrix {
                    row_count,
                    col_count,
                }) => {
                    if row_count > max_height {
                        max_height = row_count;
                    }
                }
                _ => {}
            }
        }
        self.rendered_row_height = max_height.max(result_row_height);
        // "- 1" so if it is even, it always appear higher
        self.vert_align_offset = (self.rendered_row_height - 1) / 2;
        self.editor_y_to_vert_align[self.editor_pos.row] = self.vert_align_offset;
    }

    fn token_render_done(&mut self, editor_len: usize, render_len: usize, x_offset: isize) {
        self.render_pos.column += render_len;
        self.editor_pos.column += editor_len;
        self.cursor_render_x_offset += x_offset;
    }
}

impl<'a> NoteCalcApp<'a> {
    pub fn new(client_width: usize) -> NoteCalcApp<'a> {
        let prefixes: &'static UnitPrefixes = Box::leak(Box::new(create_prefixes()));
        let units = Units::new(&prefixes);
        let mut editor_content = EditorContent::new(MAX_EDITOR_WIDTH);
        NoteCalcApp {
            prev_cursor_pos: Pos::from_row_column(0, 0),
            line_reference_chooser: None,
            client_width,
            prefixes,
            units,
            editor: Editor::new(&mut editor_content),
            editor_content,
            result_buffer: [0; 1024],
            matrix_editing: None,
            editor_click: None,
            editor_objects: Vec::with_capacity(8),
            line_id_generator: 1,
            has_result_bitset: 0,
            result_gutter_x: NoteCalcApp::calc_result_gutter_x(None, client_width),
            right_gutter_is_dragged: false,
        }
    }

    pub fn set_normalized_content(&mut self, text: &str) {
        self.editor_content.set_content(text);
        self.editor.set_cursor_pos_r_c(0, 0);
        for (i, data) in self.editor_content.data_mut().iter_mut().enumerate() {
            data.line_id = i + 1;
        }
        self.line_id_generator = self.editor_content.line_count() + 1;
    }

    pub fn end_matrix_editing(&mut self, new_cursor_pos: Option<Pos>) {
        let mat_editor = {
            let mut mat_editor = self.matrix_editing.as_mut().unwrap();
            mat_editor.save_editor_content();
            mat_editor
        };
        let mut concat = String::with_capacity(32);
        concat.push('[');
        for row_i in 0..mat_editor.row_count {
            if row_i > 0 {
                concat.push(';');
            }
            for col_i in 0..mat_editor.col_count {
                if col_i > 0 {
                    concat.push(',');
                }
                let cell_str = &mat_editor.cell_strings[row_i * mat_editor.col_count + col_i];
                concat += &cell_str;
            }
        }
        concat.push(']');
        let selection = Selection::range(
            Pos::from_row_column(mat_editor.row_index, mat_editor.start_text_index),
            Pos::from_row_column(mat_editor.row_index, mat_editor.end_text_index),
        );
        self.editor.set_selection_save_col(selection);
        // TODO: máshogy oldd meg, mert ez modositja az undo stacket is
        // és az miért baj, legalább tudom ctrl z-zni a mátrix edition-t
        self.editor.handle_input(
            EditorInputEvent::Del,
            InputModifiers::none(),
            &mut self.editor_content,
        );
        self.editor.handle_input(
            EditorInputEvent::Text(concat),
            InputModifiers::none(),
            &mut self.editor_content,
        );
        self.matrix_editing = None;

        if let Some(new_cursor_pos) = new_cursor_pos {
            self.editor
                .set_selection_save_col(Selection::single(new_cursor_pos));
        }
        self.editor.blink_cursor();
    }

    const SUM_VARIABLE_INDEX: usize = 0;

    pub fn render<'b>(&'b mut self) -> RenderBuckets<'b> {
        let mut r = RenderPass::new(self.client_width, self.result_gutter_x);

        // TODO: improve vec alloc
        let mut render_buckets = RenderBuckets::new();
        let mut result_buffer_index = 0;
        let mut results: SmallVec<[Option<CalcResult>; MAX_LINE_COUNT]> =
            SmallVec::with_capacity(MAX_LINE_COUNT);

        self.editor_objects.clear();

        // TODO avoid alloc
        let mut vars: Vec<(&[char], CalcResult)> = Vec::with_capacity(32);
        vars.push((&['s', 'u', 'm'], CalcResult::zero()));
        let mut sum_is_null = true;

        self.has_result_bitset = 0;

        for line in self.editor_content.lines().take(MAX_LINE_COUNT) {
            r.new_line_started();

            if line.starts_with(&['-', '-']) || line.starts_with(&['\'']) {
                if line.starts_with(&['-', '-']) {
                    sum_is_null = true;
                }
                NoteCalcApp::render_simple_text_line(
                    line,
                    &mut r,
                    &mut render_buckets,
                    &mut self.editor_click,
                );
                results.push(None);
            } else {
                // TODO optimize vec allocations
                let mut tokens = Vec::with_capacity(128);
                TokenParser::parse_line(line, &vars, &mut tokens, &self.units);

                let mut shunting_output_stack = Vec::with_capacity(128);
                ShuntingYard::shunting_yard(&mut tokens, &mut shunting_output_stack);

                let result_row_height = NoteCalcApp::evaluate_tokens(
                    &mut self.has_result_bitset,
                    &mut vars,
                    r.editor_pos.row,
                    &self.editor_content,
                    &mut results,
                    &mut shunting_output_stack,
                    line,
                    &mut sum_is_null,
                );

                r.calc_rendered_row_height(&tokens, result_row_height);

                // Todo: refactor the parameters into a struct
                NoteCalcApp::render_tokens(
                    &tokens,
                    &mut r,
                    &mut render_buckets,
                    &mut self.editor_objects,
                    &self.editor,
                    &self.matrix_editing,
                    &mut self.editor_click,
                    &vars,
                    &self.units,
                );
                NoteCalcApp::handle_click_after_last_token(&mut self.editor_click, &mut r);
            }

            NoteCalcApp::render_wrap_dots(&mut render_buckets, &r);

            NoteCalcApp::draw_line_ref_chooser(
                &mut render_buckets,
                &r,
                &self.line_reference_chooser,
            );

            NoteCalcApp::highlight_current_line_and_draw_cursor(
                &mut render_buckets,
                &r,
                &self.editor,
                &self.matrix_editing,
            );

            match self.editor_content.get_data(r.editor_pos.row).result_format {
                ResultFormat::Hex => {
                    render_buckets.operators.push(RenderUtf8TextMsg {
                        text: &['0', 'x'],
                        row: r.render_pos.row,
                        column: self.result_gutter_x + 1,
                    });
                }
                ResultFormat::Bin => {
                    render_buckets.operators.push(RenderUtf8TextMsg {
                        text: &['0', 'b'],
                        row: r.render_pos.row,
                        column: self.result_gutter_x + 1,
                    });
                }
                ResultFormat::Dec => {}
            }

            r.line_render_ended();
        }

        // result gutter
        render_buckets.set_color(Layer::BehindText, 0xD2D2D2_FF);
        render_buckets.draw_rect(
            Layer::BehindText,
            self.result_gutter_x,
            0,
            RenderPass::RIGHT_GUTTER_WIDTH,
            64,
        );
        // result background
        render_buckets.set_color(Layer::BehindText, 0xF2F2F2_FF);
        render_buckets.draw_rect(
            Layer::BehindText,
            self.result_gutter_x + RenderPass::RIGHT_GUTTER_WIDTH,
            0,
            r.current_result_width,
            64,
        );

        if let Some(editor_obj) =
            self.is_pos_inside_an_obj(self.editor.get_selection().get_cursor_pos())
        {
            match editor_obj.typ {
                EditorObjectType::Matrix {
                    row_count,
                    col_count,
                } => {
                    if self.matrix_editing.is_none() && !self.editor.get_selection().is_range() {
                        self.matrix_editing = Some(MatrixEditing::new(
                            row_count,
                            col_count,
                            &self.editor_content.get_line_chars(editor_obj.row)
                                [editor_obj.start_x..editor_obj.end_x],
                            editor_obj.row,
                            editor_obj.start_x,
                            editor_obj.end_x,
                            self.prev_cursor_pos,
                        ));
                    }
                }
                EditorObjectType::LineReference => {}
            }
        }

        match r.clicked_editor_pos {
            Some(Click::Simple(pos)) => {
                self.editor
                    .handle_click(pos.column, pos.row, &self.editor_content);
                self.editor.blink_cursor();
            }
            Some(Click::Drag(pos)) => {
                self.editor
                    .handle_drag(pos.column, pos.row, &self.editor_content);
                self.editor.blink_cursor();
            }
            None => {}
        }

        // gutter
        render_buckets.set_color(Layer::BehindText, 0xF2F2F2_FF);
        render_buckets.draw_rect(Layer::BehindText, 0, 0, LEFT_GUTTER_WIDTH, 255);

        // line numbers
        render_buckets.set_color(Layer::BehindText, 0xADADAD_FF);
        for i in 0..self.editor_content.line_count().min(MAX_LINE_COUNT) {
            render_buckets.custom_commands[Layer::BehindText as usize].push(
                OutputMessage::RenderUtf8Text(RenderUtf8TextMsg {
                    text: &(LINE_NUM_CONSTS[i][..]),
                    row: r.editor_y_to_render_y[i] + r.editor_y_to_vert_align[i],
                    column: 1,
                }),
            )
        }

        // selected text
        render_buckets.set_color(Layer::BehindText, 0xA6D2FF_FF);
        if self.editor.get_selection().is_range() {
            let start = self.editor.get_selection().get_first();
            let end = self.editor.get_selection().get_second();
            if end.row > start.row {
                // first line
                let height = r
                    .editor_y_to_render_y
                    .get(start.row + 1)
                    .map(|it| it - r.editor_y_to_render_y[start.row])
                    .unwrap_or(1);
                render_buckets.draw_rect(
                    Layer::BehindText,
                    start.column + LEFT_GUTTER_WIDTH,
                    r.editor_y_to_render_y[start.row],
                    self.editor_content
                        .line_len(start.row)
                        .min(r.current_editor_width),
                    height,
                );
                // full lines
                // let height = end.row - start.row - 1;
                for i in start.row + 1..end.row {
                    let height = r
                        .editor_y_to_render_y
                        .get(i + 1)
                        .map(|it| it - r.editor_y_to_render_y[i])
                        .unwrap_or(1);
                    render_buckets.draw_rect(
                        Layer::BehindText,
                        LEFT_GUTTER_WIDTH,
                        r.editor_y_to_render_y[i],
                        self.editor_content.line_len(i).min(r.current_editor_width),
                        height,
                    );
                }
                // last line
                let height = r
                    .editor_y_to_render_y
                    .get(end.row + 1)
                    .map(|it| {
                        if *it > r.editor_y_to_render_y[end.row] {
                            it - r.editor_y_to_render_y[end.row]
                        } else {
                            1
                        }
                    })
                    .unwrap_or(1);
                render_buckets.draw_rect(
                    Layer::BehindText,
                    LEFT_GUTTER_WIDTH,
                    r.editor_y_to_render_y[end.row],
                    end.column.min(r.current_editor_width),
                    height,
                );
            } else {
                let height = r
                    .editor_y_to_render_y
                    .get(start.row + 1)
                    .map(|it| {
                        if *it != 0 {
                            it - r.editor_y_to_render_y[start.row]
                        } else {
                            1
                        }
                    })
                    .unwrap_or(1);
                render_buckets.draw_rect(
                    Layer::BehindText,
                    start.column + LEFT_GUTTER_WIDTH,
                    r.editor_y_to_render_y[start.row],
                    (end.column - start.column).min(r.current_editor_width),
                    height,
                );
            }
            // evaluated result of selection, selected text
            if let Some(mut partial_result) = self.evaluate_selection(&vars, &results) {
                if start.row == end.row {
                    let selection_center = start.column + ((end.column - start.column) / 2);
                    partial_result.insert_str(0, "= ");
                    let result_w = partial_result.chars().count();
                    let centered_x =
                        (selection_center as isize - (result_w / 2) as isize).max(0) as usize;
                    render_buckets.set_color(Layer::AboveText, 0xAAFFAA_FF);
                    render_buckets.draw_rect(
                        Layer::AboveText,
                        LEFT_GUTTER_WIDTH + centered_x,
                        r.editor_y_to_render_y[start.row] - 1,
                        result_w,
                        1,
                    );
                    render_buckets.set_color(Layer::AboveText, 0x000000_FF);
                    render_buckets.draw_string(
                        Layer::AboveText,
                        LEFT_GUTTER_WIDTH + centered_x,
                        r.editor_y_to_render_y[start.row] - 1,
                        partial_result,
                    );
                } else {
                    partial_result.insert_str(0, "⎬ ∑ = ");
                    let result_w = partial_result.chars().count();
                    let x = (start.row..=end.row)
                        .map(|it| self.editor_content.line_len(it))
                        .max_by(|a, b| a.cmp(b))
                        .unwrap()
                        + 3;
                    let height =
                        r.editor_y_to_render_y[end.row] - r.editor_y_to_render_y[start.row] + 1;
                    render_buckets.set_color(Layer::AboveText, 0xAAFFAA_FF);
                    render_buckets.draw_rect(
                        Layer::AboveText,
                        LEFT_GUTTER_WIDTH + x,
                        r.editor_y_to_render_y[start.row],
                        result_w + 1,
                        height,
                    );
                    // draw the parenthesis
                    render_buckets.set_color(Layer::AboveText, 0x000000_FF);

                    render_buckets.draw_char(
                        Layer::AboveText,
                        LEFT_GUTTER_WIDTH + x,
                        r.editor_y_to_render_y[start.row],
                        '⎫',
                    );
                    render_buckets.draw_char(
                        Layer::AboveText,
                        LEFT_GUTTER_WIDTH + x,
                        r.editor_y_to_render_y[end.row],
                        '⎭',
                    );
                    for i in 1..height {
                        render_buckets.draw_char(
                            Layer::AboveText,
                            LEFT_GUTTER_WIDTH + x,
                            r.editor_y_to_render_y[start.row] + i,
                            '⎪',
                        );
                    }
                    // center
                    render_buckets.draw_string(
                        Layer::AboveText,
                        LEFT_GUTTER_WIDTH + x,
                        r.editor_y_to_render_y[start.row] + height / 2,
                        partial_result,
                    );
                }
            }
        }

        NoteCalcApp::render_results(
            &self.units,
            &mut render_buckets,
            &results,
            &mut self.result_buffer,
            &mut result_buffer_index,
            &self.editor_content,
            &r,
        );

        return render_buckets;
    }

    fn render_simple_text_line<'text_ptr>(
        line: &'text_ptr [char],
        mut r: &mut RenderPass,
        render_buckets: &mut RenderBuckets<'text_ptr>,
        editor_click: &mut Option<Click>,
    ) {
        r.set_fix_row_height(1);
        let text_len = line.len().min(r.current_editor_width);

        render_buckets.utf8_texts.push(RenderUtf8TextMsg {
            text: &line[0..text_len],
            row: r.render_pos.row,
            column: LEFT_GUTTER_WIDTH,
        });

        NoteCalcApp::handle_click_for_simple_token(editor_click, text_len, &mut r);

        r.token_render_done(text_len, text_len, 0);
    }

    fn render_tokens<'text_ptr, 'units>(
        tokens: &[Token<'text_ptr, 'units>],
        mut r: &mut RenderPass,
        render_buckets: &mut RenderBuckets<'text_ptr>,
        editor_objects: &mut Vec<EditorObject>,
        editor: &Editor,
        matrix_editing: &Option<MatrixEditing>,
        editor_click: &mut Option<Click>,
        vars: &[(&[char], CalcResult)],
        units: &'units Units<'units>,
    ) {
        let cursor_pos = editor.get_selection().get_cursor_pos();
        let need_matrix_renderer = !editor.get_selection().is_range() || {
            let first = editor.get_selection().get_first();
            let second = editor.get_selection().get_second();
            !(first.row..=second.row).contains(&r.editor_pos.row)
        };

        let mut matrix_count_in_this_row = 0;
        let mut token_index = 0;
        while token_index < tokens.len() {
            let token = &tokens[token_index];
            if let (
                TokenType::Operator(OperatorTokenType::Matrix {
                    row_count,
                    col_count,
                }),
                true,
            ) = (&token.typ, need_matrix_renderer)
            {
                matrix_count_in_this_row += 1;
                if matrix_count_in_this_row > 1 {
                    r.prev_matrix_format = None;
                }
                token_index = NoteCalcApp::render_matrix(
                    token_index,
                    &tokens,
                    *row_count,
                    *col_count,
                    &mut r,
                    render_buckets,
                    editor_objects,
                    &editor,
                    &matrix_editing,
                    editor_click,
                );
            } else if let (TokenType::LineReference { var_index }, true) =
                (&token.typ, need_matrix_renderer)
            {
                let (var_name, result) = &vars[*var_index];
                let var_name_len = var_name.len();
                let result_str = render_result(
                    &units,
                    &result,
                    &ResultFormat::Dec,
                    // TODO: ide kellene a result.there_was_unit_conversion
                    false,
                    2,
                );
                editor_objects.push(EditorObject {
                    typ: EditorObjectType::LineReference,
                    row: r.editor_pos.row,
                    start_x: r.editor_pos.column,
                    end_x: r.editor_pos.column + var_name_len,
                });
                let text_len = result_str.chars().count().min(
                    (r.current_editor_width as isize - r.render_pos.column as isize).max(0)
                        as usize,
                );
                render_buckets.line_ref_results.push(RenderStringMsg {
                    text: result_str[0..text_len].to_owned(),
                    row: r.render_pos.row,
                    column: r.render_pos.column + LEFT_GUTTER_WIDTH,
                });

                token_index += 1;
                r.token_render_done(
                    var_name_len,
                    text_len,
                    if cursor_pos.column > r.editor_pos.column {
                        let rendered_width = text_len as isize;
                        let diff = rendered_width - var_name_len as isize;
                        diff
                    } else {
                        0
                    },
                );
            } else {
                NoteCalcApp::draw_token(
                    token,
                    r.render_pos.column,
                    r.render_pos.row + r.vert_align_offset,
                    r.current_editor_width,
                    render_buckets,
                );

                NoteCalcApp::handle_click_for_simple_token(editor_click, token.ptr.len(), &mut r);

                token_index += 1;
                r.token_render_done(token.ptr.len(), token.ptr.len(), 0);
            }
        }
        if matrix_count_in_this_row < 1 {
            r.prev_matrix_format = None;
        }
    }

    fn render_wrap_dots(render_buckets: &mut RenderBuckets, r: &RenderPass) {
        if r.render_pos.column > r.current_editor_width {
            render_buckets.draw_char(
                Layer::AboveText,
                r.current_editor_width + LEFT_GUTTER_WIDTH,
                r.render_pos.row,
                '…',
            );
        }
    }

    fn draw_line_ref_chooser(
        render_buckets: &mut RenderBuckets,
        r: &RenderPass,
        line_reference_chooser: &Option<usize>,
    ) {
        if let Some(selection_row) = line_reference_chooser {
            if *selection_row == r.editor_pos.row {
                render_buckets.set_color(Layer::BehindText, 0xFFCCCC_FF);
                render_buckets.draw_rect(
                    Layer::BehindText,
                    0,
                    r.render_pos.row,
                    r.result_gutter_x
                        + RenderPass::RIGHT_GUTTER_WIDTH
                        + RenderPass::MIN_RESULT_PANEL_WIDTH,
                    r.rendered_row_height,
                );
            }
        }
    }

    fn highlight_current_line_and_draw_cursor(
        render_buckets: &mut RenderBuckets,
        r: &RenderPass,
        editor: &Editor,
        matrix_editing: &Option<MatrixEditing>,
    ) {
        let cursor_pos = editor.get_selection().get_cursor_pos();
        if cursor_pos.row == r.editor_pos.row {
            render_buckets.set_color(Layer::BehindText, 0xFFFFCC_C8);
            render_buckets.draw_rect(
                Layer::BehindText,
                0,
                r.render_pos.row,
                r.result_gutter_x
                    + RenderPass::RIGHT_GUTTER_WIDTH
                    + RenderPass::MIN_RESULT_PANEL_WIDTH,
                r.rendered_row_height,
            );
            // cursor is above it, so it is easier to spot
            render_buckets.set_color(Layer::AboveText, 0x000000_FF);
            if editor.is_cursor_shown()
                && matrix_editing.is_none()
                && ((cursor_pos.column as isize + r.cursor_render_x_offset) as usize)
                    < r.current_editor_width
            {
                render_buckets.draw_char(
                    Layer::AboveText,
                    ((cursor_pos.column + LEFT_GUTTER_WIDTH) as isize + r.cursor_render_x_offset)
                        as usize,
                    r.render_pos.row + r.vert_align_offset,
                    '▏',
                );
            }
        }
    }

    fn evaluate_tokens<'text_ptr, 'units>(
        has_result_bitset: &mut u64,
        vars: &mut Vec<(&'text_ptr [char], CalcResult<'units>)>,
        editor_y: usize,
        editor_content: &EditorContent<LineData>,
        results: &mut SmallVec<[Option<CalcResult<'units>>; MAX_LINE_COUNT]>,
        shunting_output_stack: &mut Vec<TokenType<'units>>,
        line: &'text_ptr [char],
        sum_is_null: &mut bool,
    ) -> usize {
        if let Some(result) = evaluate_tokens(shunting_output_stack, &vars) {
            *has_result_bitset |= 1u64 << editor_y as u64;
            let line_data = editor_content.get_data(editor_y);
            let result_row_height = match &result.result {
                CalcResult::Matrix(mat) => mat.row_count,
                _ => 1,
            };

            results.push(Some(result.result.clone()));

            if result.assignment {
                let var_name = {
                    let mut i = 0;
                    // skip whitespaces
                    while line[i].is_ascii_whitespace() {
                        i += 1;
                    }
                    let start = i;
                    // take until '='
                    while line[i] != '=' {
                        i += 1;
                    }
                    // remove trailing whitespaces
                    i -= 1;
                    while line[i].is_ascii_whitespace() {
                        i -= 1;
                    }
                    let end = i;
                    &line[start..=end]
                };
                if *sum_is_null {
                    vars[NoteCalcApp::SUM_VARIABLE_INDEX].1 = result.result.clone();
                    *sum_is_null = false;
                } else {
                    vars[NoteCalcApp::SUM_VARIABLE_INDEX].1 =
                        add_op(&vars[NoteCalcApp::SUM_VARIABLE_INDEX].1, &result.result)
                            .unwrap_or(CalcResult::zero());
                }
                // variable redeclaration
                if let Some(i) = vars.iter().position(|it| it.0 == var_name) {
                    vars[i].1 = result.result;
                } else {
                    vars.push((var_name, result.result));
                }
            } else if line_data.line_id != 0 {
                let line_id = line_data.line_id;
                {
                    if *sum_is_null {
                        vars[NoteCalcApp::SUM_VARIABLE_INDEX].1 = result.result.clone();
                        *sum_is_null = false;
                    } else {
                        vars[NoteCalcApp::SUM_VARIABLE_INDEX].1 =
                            add_op(&vars[NoteCalcApp::SUM_VARIABLE_INDEX].1, &result.result)
                                .unwrap_or(CalcResult::zero());
                    }
                }
                vars.push((STATIC_LINE_IDS[line_id], result.result));
            } else {
                // TODO extract the sum addition
                if *sum_is_null {
                    vars[NoteCalcApp::SUM_VARIABLE_INDEX].1 = result.result.clone();
                    *sum_is_null = false;
                } else {
                    vars[NoteCalcApp::SUM_VARIABLE_INDEX].1 =
                        add_op(&vars[NoteCalcApp::SUM_VARIABLE_INDEX].1, &result.result)
                            .unwrap_or(CalcResult::zero());
                }
            }
            return result_row_height;
        } else {
            results.push(None);
            return 0;
        };
    }

    fn render_matrix<'text_ptr, 'units>(
        token_index: usize,
        tokens: &[Token<'text_ptr, 'units>],
        row_count: usize,
        col_count: usize,
        mut r: &mut RenderPass,
        render_buckets: &mut RenderBuckets<'text_ptr>,
        editor_objects: &mut Vec<EditorObject>,
        editor: &Editor,
        matrix_editing: &Option<MatrixEditing>,
        editor_click: &mut Option<Click>,
    ) -> usize {
        let mut text_width = 0;
        let mut end_token_index = token_index;
        while tokens[end_token_index].typ != TokenType::Operator(OperatorTokenType::BracketClose) {
            text_width += tokens[end_token_index].ptr.len();
            end_token_index += 1;
        }
        // ignore the bracket as well
        text_width += 1;
        end_token_index += 1;

        editor_objects.push(EditorObject {
            typ: EditorObjectType::Matrix {
                row_count,
                col_count,
            },
            row: r.editor_pos.row,
            start_x: r.editor_pos.column,
            end_x: r.editor_pos.column + text_width,
        });

        let cursor_pos = editor.get_selection().get_cursor_pos();
        let cursor_isnide_matrix: bool = if !editor.get_selection().is_range()
            && cursor_pos.row == r.editor_pos.row
            && cursor_pos.column > r.editor_pos.column
            && cursor_pos.column < r.editor_pos.column + text_width
        {
            // cursor is inside the matrix
            true
        } else {
            false
        };

        let new_render_x = if let (true, Some(mat_editor)) = (cursor_isnide_matrix, matrix_editing)
        {
            mat_editor.render(
                r.render_pos.column,
                r.render_pos.row,
                r.current_editor_width,
                render_buckets,
                r.rendered_row_height,
            )
        } else {
            NoteCalcApp::render_matrix_obj(
                r.render_pos.column,
                r.render_pos.row,
                r.current_editor_width,
                row_count,
                col_count,
                &tokens[token_index..],
                render_buckets,
                r.rendered_row_height,
            )
        };

        let rendered_width = (new_render_x - r.render_pos.column);
        let x_diff = if cursor_pos.row == r.editor_pos.row
            && cursor_pos.column >= r.editor_pos.column + text_width
        {
            let diff = rendered_width as isize - text_width as isize;
            diff
        } else {
            0
        };
        NoteCalcApp::handle_click_for_matrix_token(editor_click, new_render_x, &mut r);

        r.token_render_done(text_width, rendered_width, x_diff);
        return end_token_index;
    }

    fn handle_click_for_matrix_token(
        editor_click: &mut Option<Click>,
        new_render_x: usize,
        r: &mut RenderPass,
    ) {
        match *editor_click {
            Some(Click::Simple(clicked_pos)) | Some(Click::Drag(clicked_pos)) => {
                if (r.render_pos.row..r.render_pos.row + r.rendered_row_height)
                    .contains(&clicked_pos.row)
                    && new_render_x >= clicked_pos.column
                {
                    let fixed_pos = Pos::from_row_column(r.editor_pos.row, r.editor_pos.column + 1);
                    r.clicked_editor_pos = Some(editor_click.as_ref().unwrap().with_pos(fixed_pos));
                    *editor_click = None;
                }
            }
            None => {}
        }
    }

    fn handle_click_for_simple_token(
        editor_click: &mut Option<Click>,
        token_len: usize,
        r: &mut RenderPass,
    ) {
        match editor_click {
            Some(Click::Simple(clicked_pos)) | Some(Click::Drag(clicked_pos)) => {
                let new_x = r.render_pos.column + token_len;
                if r.render_pos.row + r.rendered_row_height > clicked_pos.row
                    && new_x >= clicked_pos.column
                {
                    let click_offset_x = (clicked_pos.column as isize
                        - r.render_pos.column as isize)
                        .max(0) as usize;
                    let fixed_pos = Pos::from_row_column(
                        r.editor_pos.row,
                        r.editor_pos.column + click_offset_x,
                    );

                    r.clicked_editor_pos = Some(editor_click.as_ref().unwrap().with_pos(fixed_pos));
                    *editor_click = None;
                }
            }
            None => {}
        }
    }

    fn handle_click_after_last_token(editor_click: &mut Option<Click>, r: &mut RenderPass) {
        match editor_click {
            Some(Click::Simple(clicked_pos)) | Some(Click::Drag(clicked_pos)) => {
                if r.render_pos.row + r.rendered_row_height > clicked_pos.row {
                    let fixed_pos = Pos::from_row_column(r.editor_pos.row, r.editor_pos.column);
                    r.clicked_editor_pos = Some(editor_click.as_ref().unwrap().with_pos(fixed_pos));
                    *editor_click = None;
                }
            }
            _ => {}
        }
    }

    fn evaluate_selection(
        &self,
        vars: &Vec<(&[char], CalcResult)>,
        results: &SmallVec<[Option<CalcResult>; MAX_LINE_COUNT]>,
    ) -> Option<String> {
        let sel = self.editor.get_selection();
        // TODO optimize vec allocations
        let mut tokens = Vec::with_capacity(128);
        if sel.start.row == sel.end.unwrap().row {
            if let Some(selected_text) =
                Editor::get_selected_text_single_line(sel, &self.editor_content)
            {
                if let Some(result) = self.evaluate_text(selected_text, &vars, &mut tokens) {
                    if result.there_was_operation {
                        let result_str = render_result(
                            &self.units,
                            &result.result,
                            &self.editor_content.get_data(sel.start.row).result_format,
                            result.there_was_unit_conversion,
                            4,
                        );
                        return Some(result_str);
                    }
                }
            }
        } else {
            let mut sum: Option<&CalcResult> = None;
            let mut tmp_sum = CalcResult::hack_empty();
            for row_index in sel.get_first().row..=sel.get_second().row {
                if let Some(line_result) = &results[row_index] {
                    if let Some(sum_r) = &sum {
                        if let Some(add_result) = add_op(sum_r, &line_result) {
                            tmp_sum = add_result;
                            sum = Some(&tmp_sum);
                        } else {
                            return None; // don't show anything if can't add all the rows
                        }
                    } else {
                        sum = Some(&line_result);
                    }
                }
            }
            if let Some(sum) = sum {
                let result_str = render_result(
                    &self.units,
                    sum,
                    &self.editor_content.get_data(sel.start.row).result_format,
                    false,
                    4,
                );
                return Some(result_str);
            }
        }
        return None;
    }

    fn evaluate_text<'text_ptr, 'units>(
        &'units self,
        text: &'text_ptr [char],
        vars: &'units Vec<(&'text_ptr [char], CalcResult)>,
        tokens: &mut Vec<Token<'text_ptr, 'units>>,
    ) -> Option<EvaluationResult> {
        TokenParser::parse_line(text, vars, tokens, &self.units);
        let mut shunting_output_stack = Vec::with_capacity(4);
        ShuntingYard::shunting_yard(tokens, &mut shunting_output_stack);
        return evaluate_tokens(&mut shunting_output_stack, &vars);
    }

    fn render_matrix_obj<'text_ptr, 'units>(
        mut render_x: usize,
        mut render_y: usize,
        current_editor_width: usize,
        row_count: usize,
        col_count: usize,
        tokens: &[Token<'text_ptr, 'units>],
        render_buckets: &mut RenderBuckets<'text_ptr>,
        rendered_row_height: usize,
    ) -> usize {
        let vert_align_offset = (rendered_row_height - row_count) / 2;

        render_buckets.operators.push(RenderUtf8TextMsg {
            text: &['⎡'],
            row: render_y + vert_align_offset,
            column: render_x + LEFT_GUTTER_WIDTH,
        });
        for i in 1..row_count - 1 {
            render_buckets.operators.push(RenderUtf8TextMsg {
                text: &['⎢'],
                row: render_y + i + vert_align_offset,
                column: render_x + LEFT_GUTTER_WIDTH,
            });
        }
        render_buckets.operators.push(RenderUtf8TextMsg {
            text: &['⎣'],
            row: render_y + row_count - 1 + vert_align_offset,
            column: render_x + LEFT_GUTTER_WIDTH,
        });
        render_x += 1;

        let mut tokens_per_cell = {
            // TODO smallvec
            let mut tokens_per_cell: [MaybeUninit<&[Token]>; 32] =
                unsafe { MaybeUninit::uninit().assume_init() };

            let mut start_token_index = 0;
            let mut cell_index = 0;
            let mut can_ignore_ws = true;
            for (token_index, token) in tokens.iter().enumerate() {
                if token.typ == TokenType::Operator(OperatorTokenType::BracketClose) {
                    tokens_per_cell[cell_index] =
                        MaybeUninit::new(&tokens[start_token_index..token_index]);
                    break;
                } else if token.typ
                    == TokenType::Operator(OperatorTokenType::Matrix {
                        row_count,
                        col_count,
                    })
                    || token.typ == TokenType::Operator(OperatorTokenType::BracketOpen)
                {
                    // skip them
                    start_token_index = token_index + 1;
                } else if can_ignore_ws && token.ptr[0].is_ascii_whitespace() {
                    start_token_index = token_index + 1;
                } else if token.typ == TokenType::Operator(OperatorTokenType::Comma)
                    || token.typ == TokenType::Operator(OperatorTokenType::Semicolon)
                {
                    tokens_per_cell[cell_index] =
                        MaybeUninit::new(&tokens[start_token_index..token_index]);
                    start_token_index = token_index + 1;
                    cell_index += 1;
                    can_ignore_ws = true;
                } else {
                    can_ignore_ws = false;
                }
            }
            unsafe { std::mem::transmute::<_, [&[Token]; 32]>(tokens_per_cell) }
        };

        for col_i in 0..col_count {
            if render_x >= current_editor_width {
                return render_x;
            }
            let max_width: usize = (0..row_count)
                .map(|row_i| {
                    tokens_per_cell[row_i * col_count + col_i]
                        .iter()
                        .map(|it| it.ptr.len())
                        .sum()
                })
                .max()
                .unwrap();
            for row_i in 0..row_count {
                let tokens = &tokens_per_cell[row_i * col_count + col_i];
                let len: usize = tokens.iter().map(|it| it.ptr.len()).sum();
                let offset_x = max_width - len;
                let mut local_x = 0;
                for token in tokens.iter() {
                    NoteCalcApp::draw_token(
                        token,
                        render_x + offset_x + local_x,
                        render_y + row_i + vert_align_offset,
                        current_editor_width,
                        render_buckets,
                    );
                    local_x += token.ptr.len();
                }
            }
            render_x += if col_i + 1 < col_count {
                max_width + 2
            } else {
                max_width
            };
        }

        render_buckets.operators.push(RenderUtf8TextMsg {
            text: &['⎤'],
            row: render_y + vert_align_offset,
            column: render_x + LEFT_GUTTER_WIDTH,
        });
        for i in 1..row_count - 1 {
            render_buckets.operators.push(RenderUtf8TextMsg {
                text: &['⎥'],
                row: render_y + i + vert_align_offset,
                column: render_x + LEFT_GUTTER_WIDTH,
            });
        }
        render_buckets.operators.push(RenderUtf8TextMsg {
            text: &['⎦'],
            row: render_y + row_count - 1 + vert_align_offset,
            column: render_x + LEFT_GUTTER_WIDTH,
        });
        render_x += 1;

        render_x
    }

    fn render_matrix_result<'text_ptr, 'units>(
        units: &Units<'units>,
        mut render_x: usize,
        mut render_y: usize,
        mat: &MatrixData<'units>,
        render_buckets: &mut RenderBuckets<'text_ptr>,
        prev_mat_result_lengths: &Option<ResultLengths>,
    ) {
        render_buckets.operators.push(RenderUtf8TextMsg {
            text: &['⎡'],
            row: render_y,
            column: render_x,
        });
        for i in 1..mat.row_count - 1 {
            render_buckets.operators.push(RenderUtf8TextMsg {
                text: &['⎢'],
                row: render_y + i,
                column: render_x,
            });
        }
        render_buckets.operators.push(RenderUtf8TextMsg {
            text: &['⎣'],
            row: render_y + mat.row_count - 1,
            column: render_x,
        });
        render_x += 1;

        let mut cells_strs = {
            let mut tokens_per_cell: SmallVec<[String; 32]> = SmallVec::with_capacity(32);

            let mut cell_index = 0;
            for cell in mat.cells.iter() {
                let result_str = render_result(units, cell, &ResultFormat::Dec, false, 4);
                tokens_per_cell.push(result_str);
                cell_index += 1;
            }
            tokens_per_cell
        };

        let max_lengths = {
            let mut max_lengths = ResultLengths {
                int_part_len: prev_mat_result_lengths
                    .as_ref()
                    .map(|it| it.int_part_len)
                    .unwrap_or(0),
                frac_part_len: prev_mat_result_lengths
                    .as_ref()
                    .map(|it| it.frac_part_len)
                    .unwrap_or(0),
                unit_part_len: prev_mat_result_lengths
                    .as_ref()
                    .map(|it| it.unit_part_len)
                    .unwrap_or(0),
            };
            for cell_str in &cells_strs {
                let lengths = get_int_frac_part_len(cell_str);
                max_lengths.set_max(&lengths);
            }
            max_lengths
        };
        for col_i in 0..mat.col_count {
            for row_i in 0..mat.row_count {
                let cell_str = &cells_strs[row_i * mat.col_count + col_i];
                let lengths = get_int_frac_part_len(cell_str);
                // Draw integer part
                let offset_x = max_lengths.int_part_len - lengths.int_part_len;
                render_buckets.draw_string(
                    Layer::AboveText,
                    render_x + offset_x,
                    render_y + row_i,
                    // TOOD nem kell clone, csinálj iter into vhogy
                    cell_str[0..lengths.int_part_len].to_owned(),
                );

                let mut frac_offset_x = 0;
                if lengths.frac_part_len > 0 {
                    render_buckets.draw_string(
                        Layer::AboveText,
                        render_x + offset_x + lengths.int_part_len,
                        render_y + row_i,
                        // TOOD nem kell clone, csinálj iter into vhogy
                        cell_str
                            [lengths.int_part_len..lengths.int_part_len + lengths.frac_part_len]
                            .to_owned(),
                    )
                } else if max_lengths.frac_part_len > 0 {
                    render_buckets.draw_char(
                        Layer::AboveText,
                        render_x + offset_x + lengths.int_part_len,
                        render_y + row_i,
                        '.',
                    );
                    frac_offset_x = 1;
                }
                for i in 0..max_lengths.frac_part_len - lengths.frac_part_len - frac_offset_x {
                    render_buckets.draw_char(
                        Layer::AboveText,
                        render_x
                            + offset_x
                            + lengths.int_part_len
                            + lengths.frac_part_len
                            + frac_offset_x
                            + i,
                        render_y + row_i,
                        '0',
                    )
                }
                if lengths.unit_part_len > 0 {
                    render_buckets.draw_string(
                        Layer::AboveText,
                        render_x + offset_x + lengths.int_part_len + max_lengths.frac_part_len + 1,
                        render_y + row_i,
                        // TOOD nem kell clone, csinálj iter into vhogy
                        // +1, skip space
                        cell_str[lengths.int_part_len + lengths.frac_part_len + 1..].to_owned(),
                    )
                }
            }
            render_x += if col_i + 1 < mat.col_count {
                (max_lengths.int_part_len + max_lengths.frac_part_len + max_lengths.unit_part_len)
                    + 2
            } else {
                (max_lengths.int_part_len + max_lengths.frac_part_len + max_lengths.unit_part_len)
            };
        }

        render_buckets.operators.push(RenderUtf8TextMsg {
            text: &['⎤'],
            row: render_y,
            column: render_x,
        });
        for i in 1..mat.row_count - 1 {
            render_buckets.operators.push(RenderUtf8TextMsg {
                text: &['⎥'],
                row: render_y + i,
                column: render_x,
            });
        }
        render_buckets.operators.push(RenderUtf8TextMsg {
            text: &['⎦'],
            row: render_y + mat.row_count - 1,
            column: render_x,
        });
    }

    fn calc_matrix_max_lengths<'text_ptr, 'units>(
        units: &Units<'units>,
        mat: &MatrixData<'units>,
    ) -> ResultLengths {
        let mut cells_strs = {
            let mut tokens_per_cell: SmallVec<[String; 32]> = SmallVec::with_capacity(32);

            let mut cell_index = 0;
            for cell in mat.cells.iter() {
                let result_str = render_result(units, cell, &ResultFormat::Dec, false, 4);
                tokens_per_cell.push(result_str);
                cell_index += 1;
            }
            tokens_per_cell
        };
        let max_lengths = {
            let mut max_lengths = ResultLengths {
                int_part_len: 0,
                frac_part_len: 0,
                unit_part_len: 0,
            };
            for cell_str in &cells_strs {
                let lengths = get_int_frac_part_len(cell_str);
                max_lengths.set_max(&lengths);
            }
            max_lengths
        };
        return max_lengths;
    }

    fn render_results<'text_ptr, 'units>(
        units: &Units<'units>,
        render_buckets: &mut RenderBuckets<'text_ptr>,
        results: &[Option<CalcResult<'units>>],
        result_buffer: &'text_ptr mut [u8],
        result_buffer_index: &mut usize,
        editor_content: &EditorContent<LineData>,
        r: &RenderPass,
    ) {
        let mut result_ranges: SmallVec<[Option<Range<usize>>; MAX_LINE_COUNT]> =
            SmallVec::with_capacity(MAX_LINE_COUNT);

        let mut max_lengths = ResultLengths {
            int_part_len: 0,
            frac_part_len: 0,
            unit_part_len: 0,
        };
        let mut prev_result_matrix_length = None;
        for (editor_y, result) in results.iter().enumerate() {
            if let Some(result) = result {
                match &result {
                    CalcResult::Matrix(mat) => {
                        if prev_result_matrix_length.is_none() {
                            prev_result_matrix_length =
                                NoteCalcApp::calc_consecutive_matrices_max_lengths(
                                    units,
                                    &results[editor_y..],
                                );
                        }
                        NoteCalcApp::render_matrix_result(
                            units,
                            r.result_gutter_x + RenderPass::RIGHT_GUTTER_WIDTH,
                            r.editor_y_to_render_y[editor_y],
                            mat,
                            render_buckets,
                            &prev_result_matrix_length,
                        );
                        result_ranges.push(None);
                    }
                    _ => {
                        prev_result_matrix_length = None;
                        let start = *result_buffer_index;
                        let mut c = Cursor::new(&mut result_buffer[start..]);
                        render_result_into(
                            &units,
                            &result,
                            &editor_content.get_data(editor_y).result_format,
                            false,
                            &mut c,
                            4,
                        );
                        let len = c.position() as usize;
                        let range = start..start + len;
                        let s =
                            unsafe { std::str::from_utf8_unchecked(&result_buffer[range.clone()]) };
                        let lengths = get_int_frac_part_len(s);
                        max_lengths.set_max(&lengths);
                        result_ranges.push(Some((range)));
                        *result_buffer_index += len;
                    }
                };
            } else {
                result_ranges.push(None);
            }
        }
        for (editor_y, result_range) in result_ranges.iter().enumerate() {
            if let Some(result_range) = result_range {
                let s =
                    unsafe { std::str::from_utf8_unchecked(&result_buffer[result_range.clone()]) };
                let lengths = get_int_frac_part_len(s);
                let from = result_range.start;
                let row = r.editor_y_to_render_y[editor_y] + r.editor_y_to_vert_align[editor_y];
                let offset_x = max_lengths.int_part_len - lengths.int_part_len;
                let x = r.result_gutter_x + RenderPass::RIGHT_GUTTER_WIDTH + offset_x;
                render_buckets.ascii_texts.push(RenderAsciiTextMsg {
                    text: &result_buffer[from..from + lengths.int_part_len],
                    row,
                    column: x,
                });
                if lengths.frac_part_len > 0 {
                    let from = result_range.start + lengths.int_part_len;
                    render_buckets.ascii_texts.push(RenderAsciiTextMsg {
                        text: &result_buffer[from..from + lengths.frac_part_len],
                        row,
                        column: x + lengths.int_part_len,
                    });
                }
                if lengths.unit_part_len > 0 {
                    let from =
                        result_range.start + lengths.int_part_len + lengths.frac_part_len + 1;
                    render_buckets.ascii_texts.push(RenderAsciiTextMsg {
                        text: &result_buffer[from..result_range.end],
                        row,
                        column: x + lengths.int_part_len + lengths.frac_part_len + 1,
                    });
                }
            }
        }
    }

    fn calc_consecutive_matrices_max_lengths<'text_ptr, 'units>(
        units: &Units<'units>,
        results: &[Option<CalcResult<'units>>],
    ) -> Option<ResultLengths> {
        let mut max_lengths: Option<ResultLengths> = None;
        for result in results.iter() {
            match result {
                Some(CalcResult::Matrix(mat)) => {
                    let lengths = NoteCalcApp::calc_matrix_max_lengths(units, mat);
                    if let Some(max_lengths) = &mut max_lengths {
                        max_lengths.set_max(&lengths);
                    } else {
                        max_lengths = Some(lengths);
                    }
                }
                _ => {
                    break;
                }
            }
        }
        return max_lengths;
    }

    fn draw_token<'text_ptr, 'units>(
        token: &Token<'text_ptr, 'units>,
        render_x: usize,
        render_y: usize,
        current_editor_width: usize,
        render_buckets: &mut RenderBuckets<'text_ptr>,
    ) {
        let dst = match &token.typ {
            TokenType::StringLiteral => &mut render_buckets.utf8_texts,
            TokenType::Variable { .. } => &mut render_buckets.variable,
            TokenType::LineReference { .. } => &mut render_buckets.variable,
            TokenType::NumberLiteral(_) => &mut render_buckets.numbers,
            TokenType::Operator(op_type) => match op_type {
                OperatorTokenType::Unit(_) => &mut render_buckets.units,
                _ => &mut render_buckets.operators,
            },
        };
        let text_len = token
            .ptr
            .len()
            .min((current_editor_width as isize - render_x as isize).max(0) as usize);
        dst.push(RenderUtf8TextMsg {
            text: &token.ptr[0..text_len],
            row: render_y,
            column: render_x + LEFT_GUTTER_WIDTH,
        });
    }

    pub fn handle_mouse_up(&mut self, x: usize, y: usize) {
        self.right_gutter_is_dragged = false;
    }

    pub fn handle_click(&mut self, x: usize, y: usize) {
        if x < LEFT_GUTTER_WIDTH {
            // clicked on left gutter
        } else if x < self.result_gutter_x {
            self.editor_click = Some(Click::Simple(Pos::from_row_column(
                y,
                x - LEFT_GUTTER_WIDTH,
            )));
            if self.matrix_editing.is_some() {
                self.end_matrix_editing(None);
            }
        } else if x - self.result_gutter_x < RenderPass::RIGHT_GUTTER_WIDTH {
            // clicked on right gutter
            self.right_gutter_is_dragged = true;
        }
    }

    pub fn handle_drag(&mut self, x: usize, y: usize) {
        if self.right_gutter_is_dragged {
            self.result_gutter_x = NoteCalcApp::calc_result_gutter_x(Some(x), self.client_width);
        } else if x < LEFT_GUTTER_WIDTH {
            // clicked on left gutter
        } else if x - LEFT_GUTTER_WIDTH < MAX_EDITOR_WIDTH {
            self.editor_click = Some(Click::Drag(Pos::from_row_column(y, x - LEFT_GUTTER_WIDTH)));
        }
    }

    pub fn handle_resize(&mut self, new_client_width: usize) {
        self.client_width = new_client_width;
        self.result_gutter_x =
            NoteCalcApp::calc_result_gutter_x(Some(self.result_gutter_x), new_client_width);
    }

    fn calc_result_gutter_x(current_x: Option<usize>, client_width: usize) -> usize {
        return (if let Some(current_x) = current_x {
            current_x
        } else {
            (LEFT_GUTTER_WIDTH + MAX_EDITOR_WIDTH)
        })
        .min(client_width - (RenderPass::RIGHT_GUTTER_WIDTH + RenderPass::MIN_RESULT_PANEL_WIDTH));
    }

    pub fn handle_time(&mut self, now: u32) -> bool {
        return if let Some(mat_editor) = &mut self.matrix_editing {
            mat_editor.editor.handle_tick(now)
        } else {
            self.editor.handle_tick(now)
        };
    }

    pub fn get_normalized_content(&self) -> String {
        let mut result: String = String::with_capacity(self.editor_content.line_count() * 40);
        for line in self.editor_content.lines() {
            let mut i = 0;
            'i: while i < line.len() {
                if i + 3 < line.len() && line[i] == '&' && line[i + 1] == '[' {
                    let mut end = i + 2;
                    let mut num: u32 = 0;
                    while end < line.len() {
                        if line[end] == ']' && num > 0 {
                            // which row has the id of 'num'?
                            let row_index = self
                                .editor_content
                                .data()
                                .iter()
                                .position(|it| it.line_id == num as usize)
                                .unwrap_or(0)
                                + 1; // '+1' line id cannot be 0
                            result.push('&');
                            result.push('[');
                            let mut line_id = row_index;
                            while line_id > 0 {
                                let to_insert = line_id % 10;
                                result.push((48 + to_insert as u8) as char);
                                line_id /= 10;
                            }
                            // reverse the number
                            {
                                let last_i = result.len() - 1;
                                let replace_i = if row_index > 99 {
                                    2
                                } else if row_index > 9 {
                                    1
                                } else {
                                    0
                                };
                                if replace_i != 0 {
                                    unsafe {
                                        let tmp = result.as_bytes()[last_i];
                                        result.as_bytes_mut()[last_i] =
                                            result.as_bytes()[last_i - replace_i];
                                        result.as_bytes_mut()[last_i - replace_i] = tmp;
                                    }
                                }
                            }
                            result.push(']');
                            i = end + 1;
                            continue 'i;
                        } else if let Some(digit) = line[end].to_digit(10) {
                            num = if num == 0 { digit } else { num * 10 + digit };
                        } else {
                            break;
                        }
                        end += 1;
                    }
                }
                result.push(line[i]);
                i += 1;
            }
            result.push('\n');
        }

        return result;
    }

    pub fn alt_key_released(&mut self) {
        if self.line_reference_chooser.is_none() {
            return;
        }
        let cur_row = self.editor.get_selection().get_cursor_pos().row;
        let row_index = self.line_reference_chooser.unwrap();
        self.line_reference_chooser = None;
        if cur_row == row_index || (self.has_result_bitset & (1u64 << row_index as u64)) == 0 {
            return;
        }
        let line_id = {
            let line_data = self.editor_content.mut_data(row_index);
            if line_data.line_id == 0 {
                line_data.line_id = self.line_id_generator;
                self.line_id_generator += 1;
            }
            line_data.line_id
        };
        let inserting_text = format!("&[{}]", line_id);
        self.editor.handle_input(
            EditorInputEvent::Text(inserting_text),
            InputModifiers::none(),
            &mut self.editor_content,
        );
    }

    pub fn handle_input(&mut self, input: EditorInputEvent, modifiers: InputModifiers) -> bool {
        if self.matrix_editing.is_none() && modifiers.alt {
            if input == EditorInputEvent::Left {
                for row_i in self.editor.get_selection().get_row_iter_incl() {
                    let new_format = match &self.editor_content.get_data(row_i).result_format {
                        ResultFormat::Bin => ResultFormat::Hex,
                        ResultFormat::Dec => ResultFormat::Bin,
                        ResultFormat::Hex => ResultFormat::Dec,
                    };
                    self.editor_content.mut_data(row_i).result_format = new_format;
                }
            } else if input == EditorInputEvent::Right {
                for row_i in self.editor.get_selection().get_row_iter_incl() {
                    let new_format = match &self.editor_content.get_data(row_i).result_format {
                        ResultFormat::Bin => ResultFormat::Dec,
                        ResultFormat::Dec => ResultFormat::Hex,
                        ResultFormat::Hex => ResultFormat::Bin,
                    };
                    self.editor_content.mut_data(row_i).result_format = new_format;
                }
            } else if input == EditorInputEvent::Up {
                let cur_pos = self.editor.get_selection().get_cursor_pos();
                self.line_reference_chooser =
                    if let Some(selector_row) = self.line_reference_chooser {
                        if selector_row > 0 {
                            Some(selector_row - 1)
                        } else {
                            Some(selector_row)
                        }
                    } else if cur_pos.row > 0 {
                        Some(cur_pos.row - 1)
                    } else {
                        None
                    }
            } else if input == EditorInputEvent::Down {
                let cur_pos = self.editor.get_selection().get_cursor_pos();
                self.line_reference_chooser =
                    if let Some(selector_row) = self.line_reference_chooser {
                        if selector_row < self.editor_content.line_count() {
                            Some(selector_row + 1)
                        } else {
                            Some(selector_row)
                        }
                    } else if cur_pos.row < self.editor_content.line_count() {
                        Some(cur_pos.row + 1)
                    } else {
                        None
                    }
            }
            false
        } else if self.matrix_editing.is_some() {
            self.handle_matrix_editor_input(input, modifiers);
            true
        } else {
            let cursor_pos = self.editor.get_selection();
            self.prev_cursor_pos = cursor_pos.get_cursor_pos();
            if input == EditorInputEvent::Backspace
                && !cursor_pos.is_range()
                && cursor_pos.start.column > 0
            {
                if let Some(index) =
                    self.index_of_editor_object_at(cursor_pos.get_cursor_pos().with_prev_col())
                {
                    // remove it
                    let obj = self.editor_objects.remove(index);
                    let sel = Selection::range(
                        Pos::from_row_column(obj.row, obj.start_x),
                        Pos::from_row_column(obj.row, obj.end_x),
                    );
                    self.editor.set_selection_save_col(sel);
                    self.editor.handle_input(
                        EditorInputEvent::Backspace,
                        InputModifiers::none(),
                        &mut self.editor_content,
                    );
                    return true;
                }
            } else if input == EditorInputEvent::Del && !cursor_pos.is_range() {
                if let Some(index) = self.index_of_editor_object_at(cursor_pos.get_cursor_pos()) {
                    // remove it
                    let obj = self.editor_objects.remove(index);
                    let sel = Selection::range(
                        Pos::from_row_column(obj.row, obj.start_x),
                        Pos::from_row_column(obj.row, obj.end_x),
                    );
                    self.editor.set_selection_save_col(sel);
                    self.editor.handle_input(
                        EditorInputEvent::Del,
                        InputModifiers::none(),
                        &mut self.editor_content,
                    );
                    return true;
                }
            } else if input == EditorInputEvent::Left
                && !cursor_pos.is_range()
                && cursor_pos.start.column > 0
            {
                if let Some(obj) =
                    self.find_editor_object_at(cursor_pos.get_cursor_pos().with_prev_col())
                {
                    if obj.typ == EditorObjectType::LineReference {
                        //  jump over it
                        self.editor.set_cursor_pos_r_c(obj.row, obj.start_x);
                        return false;
                    }
                }
            } else if input == EditorInputEvent::Right && !cursor_pos.is_range() {
                if let Some(obj) = self.find_editor_object_at(cursor_pos.get_cursor_pos()) {
                    if obj.typ == EditorObjectType::LineReference {
                        //  jump over it
                        self.editor.set_cursor_pos_r_c(obj.row, obj.end_x);
                        return false;
                    }
                }
            }

            let modified = self
                .editor
                .handle_input(input, modifiers, &mut self.editor_content);

            // asddd
            return modified;
        }
    }

    fn find_editor_object_at(&self, pos: Pos) -> Option<&EditorObject> {
        for obj in &self.editor_objects {
            if obj.row == pos.row && (obj.start_x..obj.end_x).contains(&pos.column) {
                return Some(obj);
            }
        }
        return None;
    }

    fn is_pos_inside_an_obj(&self, pos: Pos) -> Option<&EditorObject> {
        for obj in &self.editor_objects {
            if obj.row == pos.row && (obj.start_x + 1..obj.end_x).contains(&pos.column) {
                return Some(obj);
            }
        }
        return None;
    }

    fn index_of_editor_object_at(&self, pos: Pos) -> Option<usize> {
        return self
            .editor_objects
            .iter()
            .position(|obj| obj.row == pos.row && (obj.start_x..obj.end_x).contains(&pos.column));
    }

    fn handle_matrix_editor_input(&mut self, input: EditorInputEvent, modifiers: InputModifiers) {
        let mat_edit = self.matrix_editing.as_mut().unwrap();
        let cur_pos = self.editor.get_selection().get_cursor_pos();

        if input == EditorInputEvent::Esc || input == EditorInputEvent::Enter {
            self.end_matrix_editing(None);
        } else if input == EditorInputEvent::Left && mat_edit.editor.is_cursor_at_beginning() {
            if mat_edit.current_cell.column > 0 {
                mat_edit.change_cell(mat_edit.current_cell.with_prev_col());
                // cursor at end of line
                let cell_len = mat_edit.editor_content.line_len(0);
                mat_edit.editor.set_cursor_pos_r_c(0, cell_len);
            } else {
                let start_text_index = mat_edit.start_text_index;
                self.end_matrix_editing(Some(cur_pos.with_column(start_text_index)));
            }
        } else if input == EditorInputEvent::Right
            && mat_edit.editor.is_cursor_at_eol(&mat_edit.editor_content)
        {
            if mat_edit.current_cell.column + 1 < mat_edit.col_count {
                mat_edit.change_cell(mat_edit.current_cell.with_next_col());
                mat_edit.editor.set_cursor_pos_r_c(0, 0);
            } else {
                let end_text_index = mat_edit.end_text_index;
                self.end_matrix_editing(Some(cur_pos.with_column(end_text_index)));
            }
        } else if input == EditorInputEvent::Up {
            if mat_edit.current_cell.row > 0 {
                let pos = mat_edit.editor.get_selection().get_cursor_pos();
                let cols_from_right = mat_edit.editor_content.line_len(0) - pos.column;
                mat_edit.change_cell(mat_edit.current_cell.with_prev_row());
                let cell_len = mat_edit.editor_content.line_len(0);
                mat_edit
                    .editor
                    .set_cursor_pos_r_c(0, cell_len - cols_from_right.min(cell_len));
            } else {
                self.end_matrix_editing(None);
                self.editor
                    .handle_input(input, modifiers, &mut self.editor_content);
            }
        } else if input == EditorInputEvent::Down {
            if mat_edit.current_cell.row + 1 < mat_edit.row_count {
                let pos = mat_edit.editor.get_selection().get_cursor_pos();
                let cols_from_right = mat_edit.editor_content.line_len(0) - pos.column;
                mat_edit.change_cell(mat_edit.current_cell.with_next_row());
                let cell_len = mat_edit.editor_content.line_len(0);
                mat_edit
                    .editor
                    .set_cursor_pos_r_c(0, cell_len - cols_from_right.min(cell_len));
            } else {
                self.end_matrix_editing(None);
                self.editor
                    .handle_input(input, modifiers, &mut self.editor_content);
            }
        } else if input == EditorInputEvent::End {
            if mat_edit.current_cell.column != mat_edit.col_count - 1 {
                mat_edit.change_cell(mat_edit.current_cell.with_column(mat_edit.col_count - 1));
                let cell_len = mat_edit.editor_content.line_len(0);
                mat_edit.editor.set_cursor_pos_r_c(0, cell_len);
            } else {
                let end_text_index = mat_edit.end_text_index;
                self.end_matrix_editing(Some(cur_pos.with_column(end_text_index)));
                self.editor
                    .handle_input(input, modifiers, &mut self.editor_content);
            }
        } else if input == EditorInputEvent::Home {
            if mat_edit.current_cell.column != 0 {
                mat_edit.change_cell(mat_edit.current_cell.with_column(0));
                mat_edit.editor.set_cursor_pos_r_c(0, 0);
            } else {
                let start_index = mat_edit.start_text_index;
                self.end_matrix_editing(Some(cur_pos.with_column(start_index)));
                self.editor
                    .handle_input(input, modifiers, &mut self.editor_content);
            }
        } else if input == EditorInputEvent::Tab {
            if mat_edit.current_cell.column + 1 < mat_edit.col_count {
                mat_edit.change_cell(mat_edit.current_cell.with_next_col());
                mat_edit.editor.set_cursor_pos_r_c(0, 0);
            } else if mat_edit.current_cell.row + 1 < mat_edit.row_count {
                mat_edit.change_cell(mat_edit.current_cell.with_next_row().with_column(0));
                mat_edit.editor.set_cursor_pos_r_c(0, 0);
            } else {
                let end_text_index = mat_edit.end_text_index;
                self.end_matrix_editing(Some(cur_pos.with_column(end_text_index)));
            }
        } else {
            mat_edit
                .editor
                .handle_input(input, modifiers, &mut mat_edit.editor_content);
        }
    }
}

fn digit_count(n: usize) -> usize {
    let mut n = n;
    let mut count = 1;
    while n > 9 {
        count += 1;
        n = n / 10;
    }
    return count;
}

struct ResultLengths {
    int_part_len: usize,
    frac_part_len: usize,
    unit_part_len: usize,
}

impl ResultLengths {
    fn set_max(&mut self, other: &ResultLengths) {
        if self.int_part_len < other.int_part_len {
            self.int_part_len = other.int_part_len;
        }
        if self.frac_part_len < other.frac_part_len {
            self.frac_part_len = other.frac_part_len;
        }
        if self.unit_part_len < other.unit_part_len {
            self.unit_part_len = other.unit_part_len;
        }
    }
}

fn get_int_frac_part_len(cell_str: &str) -> ResultLengths {
    let mut int_part_len = 0;
    let mut frac_part_len = 0;
    let mut unit_part_len = 0;
    let mut was_point = false;
    let mut was_space = false;
    for ch in cell_str.as_bytes() {
        if *ch == b'.' {
            was_point = true;
        } else if *ch == b' ' {
            was_space = true;
        }
        if was_space {
            unit_part_len += 1;
        } else if was_point {
            frac_part_len += 1;
        } else {
            int_part_len += 1;
        }
    }
    return ResultLengths {
        int_part_len,
        frac_part_len,
        unit_part_len,
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::editor::Selection;

    #[test]
    fn bug1() {
        let mut app = NoteCalcApp::new(120);

        app.handle_input(
            EditorInputEvent::Text("[123, 2, 3; 4567981, 5, 6] * [1; 2; 3;4]".to_owned()),
            InputModifiers::none(),
        );
        app.editor
            .set_selection_save_col(Selection::single_r_c(0, 33));
        app.handle_input(EditorInputEvent::Right, InputModifiers::alt());
        app.render();
    }

    #[test]
    fn bug2() {
        let mut app = NoteCalcApp::new(120);
        app.handle_input(
            EditorInputEvent::Text("[123, 2, 3; 4567981, 5, 6] * [1; 2; 3;4]".to_owned()),
            InputModifiers::none(),
        );
        app.editor
            .set_selection_save_col(Selection::single_r_c(0, 1));
        app.handle_input(EditorInputEvent::Right, InputModifiers::alt());
        app.render();
        app.handle_input(EditorInputEvent::Down, InputModifiers::none());
        app.render();
    }

    #[test]
    fn bug3() {
        let mut app = NoteCalcApp::new(120);
        app.handle_input(
            EditorInputEvent::Text(
                "1\n\
            2+"
                .to_owned(),
            ),
            InputModifiers::none(),
        );
        app.editor
            .set_selection_save_col(Selection::single_r_c(1, 2));
        app.handle_input(EditorInputEvent::Up, InputModifiers::alt());
        app.alt_key_released();
        app.render();
    }

    #[test]
    fn remove_matrix_backspace() {
        let mut app = NoteCalcApp::new(120);
        app.handle_input(
            EditorInputEvent::Text("abcd [1,2,3;4,5,6]".to_owned()),
            InputModifiers::none(),
        );
        app.render();
        app.handle_input(EditorInputEvent::Backspace, InputModifiers::none());
        assert_eq!("abcd ", app.editor_content.get_content());
    }

    #[test]
    fn matrix_step_in_dir() {
        // from right
        {
            let mut app = NoteCalcApp::new(120);
            app.handle_input(
                EditorInputEvent::Text("abcd [1,2,3;4,5,6]".to_owned()),
                InputModifiers::none(),
            );
            app.render();
            app.handle_input(EditorInputEvent::Left, InputModifiers::none());
            app.render();
            app.handle_input(EditorInputEvent::Char('1'), InputModifiers::none());
            app.handle_input(EditorInputEvent::Enter, InputModifiers::none());
            assert_eq!("abcd [1,2,31;4,5,6]", app.editor_content.get_content());
        }
        // from left
        {
            let mut app = NoteCalcApp::new(120);
            app.handle_input(
                EditorInputEvent::Text("abcd [1,2,3;4,5,6]".to_owned()),
                InputModifiers::none(),
            );
            app.editor
                .set_selection_save_col(Selection::single_r_c(0, 5));
            app.render();
            app.handle_input(EditorInputEvent::Right, InputModifiers::none());
            app.render();
            app.handle_input(EditorInputEvent::Char('2'), InputModifiers::none());
            app.handle_input(EditorInputEvent::Enter, InputModifiers::none());
            assert_eq!("abcd [21,2,3;4,5,6]", app.editor_content.get_content());
        }
        // from below
        {
            let mut app = NoteCalcApp::new(120);
            app.handle_input(
                EditorInputEvent::Text("abcd [1,2,3;4,5,6]\naaaaaaaaaaaaaaaaaa".to_owned()),
                InputModifiers::none(),
            );
            app.editor
                .set_selection_save_col(Selection::single_r_c(1, 7));
            app.render();
            app.handle_input(EditorInputEvent::Up, InputModifiers::none());
            app.render();
            app.handle_input(EditorInputEvent::Char('2'), InputModifiers::none());
            app.handle_input(EditorInputEvent::Enter, InputModifiers::none());
            assert_eq!(
                "abcd [1,2,3;24,5,6]\naaaaaaaaaaaaaaaaaa",
                app.editor_content.get_content()
            );
        }
        // from above
        {
            let mut app = NoteCalcApp::new(120);
            app.handle_input(
                EditorInputEvent::Text("aaaaaaaaaaaaaaaaaa\nabcd [1,2,3;4,5,6]".to_owned()),
                InputModifiers::none(),
            );
            app.editor
                .set_selection_save_col(Selection::single_r_c(0, 7));
            app.render();
            app.handle_input(EditorInputEvent::Down, InputModifiers::none());
            app.render();
            app.handle_input(EditorInputEvent::Char('2'), InputModifiers::none());
            app.handle_input(EditorInputEvent::Enter, InputModifiers::none());
            assert_eq!(
                "aaaaaaaaaaaaaaaaaa\nabcd [21,2,3;4,5,6]",
                app.editor_content.get_content()
            );
        }
    }

    #[test]
    fn cursor_is_put_after_the_matrix_after_finished_editing() {
        let mut app = NoteCalcApp::new(120);
        app.handle_input(
            EditorInputEvent::Text("abcd [1,2,3;4,5,6]".to_owned()),
            InputModifiers::none(),
        );
        app.render();
        app.handle_input(EditorInputEvent::Left, InputModifiers::none());
        app.render();
        app.handle_input(EditorInputEvent::Char('6'), InputModifiers::none());
        app.render();
        app.handle_input(EditorInputEvent::Enter, InputModifiers::none());
        app.render();
        app.handle_input(EditorInputEvent::Char('9'), InputModifiers::none());
        assert_eq!("abcd [1,2,36;4,5,6]9", app.editor_content.get_content());
    }

    #[test]
    fn remove_matrix_del() {
        let mut app = NoteCalcApp::new(120);
        app.handle_input(
            EditorInputEvent::Text("abcd [1,2,3;4,5,6]".to_owned()),
            InputModifiers::none(),
        );
        app.editor
            .set_selection_save_col(Selection::single_r_c(0, 5));
        app.render();
        app.handle_input(EditorInputEvent::Del, InputModifiers::none());
        assert_eq!("abcd ", app.editor_content.get_content());
    }

    #[test]
    fn test_moving_inside_a_matrix() {
        // right to left, cursor at end
        {
            let mut app = NoteCalcApp::new(120);
            app.handle_input(
                EditorInputEvent::Text("abcd [1,2,3;4,5,6]".to_owned()),
                InputModifiers::none(),
            );
            app.render();
            app.handle_input(EditorInputEvent::Left, InputModifiers::none());
            app.render();
            app.handle_input(EditorInputEvent::Left, InputModifiers::none());
            app.handle_input(EditorInputEvent::Left, InputModifiers::none());
            app.handle_input(EditorInputEvent::Char('6'), InputModifiers::none());
            app.handle_input(EditorInputEvent::Enter, InputModifiers::none());
            app.render();
            assert_eq!("abcd [1,26,3;4,5,6]", app.editor_content.get_content());
        }
        // left to right, cursor at start
        {
            let mut app = NoteCalcApp::new(120);
            app.handle_input(
                EditorInputEvent::Text("abcd [1,2,3;4,5,6]".to_owned()),
                InputModifiers::none(),
            );
            app.editor
                .set_selection_save_col(Selection::single_r_c(0, 5));
            app.render();
            app.handle_input(EditorInputEvent::Right, InputModifiers::none());
            app.render();
            app.handle_input(EditorInputEvent::Right, InputModifiers::none());
            app.handle_input(EditorInputEvent::Right, InputModifiers::none());
            app.handle_input(EditorInputEvent::Char('6'), InputModifiers::none());
            app.handle_input(EditorInputEvent::Enter, InputModifiers::none());
            app.render();
            assert_eq!("abcd [1,62,3;4,5,6]", app.editor_content.get_content());
        }
        // vertical movement down, cursor tries to keep its position
        {
            let mut app = NoteCalcApp::new(120);
            app.handle_input(
                EditorInputEvent::Text("abcd [1111,22,3;44,55555,666]".to_owned()),
                InputModifiers::none(),
            );
            app.editor
                .set_selection_save_col(Selection::single_r_c(0, 5));
            app.render();
            app.handle_input(EditorInputEvent::Right, InputModifiers::none());
            app.render();
            // inside the matrix
            app.handle_input(EditorInputEvent::Right, InputModifiers::none());
            app.handle_input(EditorInputEvent::Right, InputModifiers::none());
            app.handle_input(EditorInputEvent::Right, InputModifiers::none());
            app.handle_input(EditorInputEvent::Down, InputModifiers::none());
            app.render();
            app.handle_input(EditorInputEvent::Char('6'), InputModifiers::none());
            app.render();
            app.handle_input(EditorInputEvent::Enter, InputModifiers::none());
            app.render();
            assert_eq!(
                "abcd [1111,22,3;464,55555,666]",
                app.editor_content.get_content()
            );
        }

        // vertical movement up, cursor tries to keep its position
        {
            let mut app = NoteCalcApp::new(120);
            app.handle_input(
                EditorInputEvent::Text("abcd [1111,22,3;44,55555,666]".to_owned()),
                InputModifiers::none(),
            );
            app.editor
                .set_selection_save_col(Selection::single_r_c(0, 5));
            app.render();
            app.handle_input(EditorInputEvent::Right, InputModifiers::none());
            app.render();
            // inside the matrix
            app.handle_input(EditorInputEvent::Right, InputModifiers::none());
            app.handle_input(EditorInputEvent::Right, InputModifiers::none());
            app.handle_input(EditorInputEvent::Right, InputModifiers::none());
            app.handle_input(EditorInputEvent::Down, InputModifiers::none());
            app.handle_input(EditorInputEvent::Up, InputModifiers::none());
            app.render();
            app.handle_input(EditorInputEvent::Char('6'), InputModifiers::none());
            app.render();
            app.handle_input(EditorInputEvent::Enter, InputModifiers::none());
            app.render();
            assert_eq!(
                "abcd [11161,22,3;44,55555,666]",
                app.editor_content.get_content()
            );
        }
    }

    #[test]
    fn test_moving_inside_a_matrix_with_tab() {
        let mut app = NoteCalcApp::new(120);
        app.handle_input(
            EditorInputEvent::Text("[1,2,3;4,5,6]".to_owned()),
            InputModifiers::none(),
        );
        app.editor
            .set_selection_save_col(Selection::single_r_c(0, 5));
        app.render();
        app.handle_input(EditorInputEvent::Right, InputModifiers::none());
        app.render();
        app.handle_input(EditorInputEvent::Tab, InputModifiers::none());
        app.handle_input(EditorInputEvent::Char('7'), InputModifiers::none());
        app.handle_input(EditorInputEvent::Tab, InputModifiers::none());
        app.handle_input(EditorInputEvent::Char('8'), InputModifiers::none());
        app.handle_input(EditorInputEvent::Tab, InputModifiers::none());
        app.handle_input(EditorInputEvent::Char('9'), InputModifiers::none());
        app.handle_input(EditorInputEvent::Tab, InputModifiers::none());
        app.handle_input(EditorInputEvent::Char('0'), InputModifiers::none());
        app.handle_input(EditorInputEvent::Tab, InputModifiers::none());
        app.handle_input(EditorInputEvent::Char('9'), InputModifiers::none());

        app.handle_input(EditorInputEvent::Enter, InputModifiers::none());
        app.render();
        assert_eq!("[1,72,83;94,05,96]", app.editor_content.get_content());
    }

    #[test]
    fn test_leaving_a_matrix_with_tab() {
        let mut app = NoteCalcApp::new(120);
        app.handle_input(
            EditorInputEvent::Text("[1,2,3;4,5,6]".to_owned()),
            InputModifiers::none(),
        );
        app.render();
        app.handle_input(EditorInputEvent::Left, InputModifiers::none());
        app.render();
        app.handle_input(EditorInputEvent::Tab, InputModifiers::none());
        app.handle_input(EditorInputEvent::Tab, InputModifiers::none());
        app.handle_input(EditorInputEvent::Tab, InputModifiers::none());
        // the next tab should leave the matrix
        app.handle_input(EditorInputEvent::Tab, InputModifiers::none());
        app.handle_input(EditorInputEvent::Char('7'), InputModifiers::none());
        app.render();
        assert_eq!("[1,2,3;4,5,6]7", app.editor_content.get_content());
    }

    #[test]
    fn end_btn_matrix() {
        {
            let mut app = NoteCalcApp::new(120);
            app.handle_input(
                EditorInputEvent::Text("abcd [1111,22,3;44,55555,666] qq".to_owned()),
                InputModifiers::none(),
            );
            app.editor
                .set_selection_save_col(Selection::single_r_c(0, 5));
            app.render();
            app.handle_input(EditorInputEvent::Right, InputModifiers::none());
            app.render();
            // inside the matrix
            app.handle_input(EditorInputEvent::End, InputModifiers::none());
            app.render();
            app.handle_input(EditorInputEvent::Char('9'), InputModifiers::none());
            app.render();
            app.handle_input(EditorInputEvent::Enter, InputModifiers::none());
            app.render();
            assert_eq!(
                "abcd [1111,22,39;44,55555,666] qq",
                app.editor_content.get_content()
            );
        }
        // pressing twice, exits the matrix
        {
            let mut app = NoteCalcApp::new(120);
            app.handle_input(
                EditorInputEvent::Text("abcd [1111,22,3;44,55555,666] qq".to_owned()),
                InputModifiers::none(),
            );
            app.editor
                .set_selection_save_col(Selection::single_r_c(0, 5));
            app.render();
            app.handle_input(EditorInputEvent::Right, InputModifiers::none());
            // inside the matrix
            app.handle_input(EditorInputEvent::End, InputModifiers::none());
            app.render();
            app.handle_input(EditorInputEvent::End, InputModifiers::none());
            app.render();
            app.handle_input(EditorInputEvent::Char('9'), InputModifiers::none());
            app.render();
            assert_eq!(
                "abcd [1111,22,3;44,55555,666] qq9",
                app.editor_content.get_content()
            );
        }
    }

    #[test]
    fn home_btn_matrix() {
        {
            let mut app = NoteCalcApp::new(120);
            app.handle_input(
                EditorInputEvent::Text("abcd [1111,22,3;44,55555,666]".to_owned()),
                InputModifiers::none(),
            );
            app.render();
            app.handle_input(EditorInputEvent::Left, InputModifiers::none());
            app.render();
            // inside the matrix
            app.handle_input(EditorInputEvent::Home, InputModifiers::none());
            app.render();
            app.handle_input(EditorInputEvent::Char('9'), InputModifiers::none());
            app.render();
            app.handle_input(EditorInputEvent::Enter, InputModifiers::none());
            app.render();
            assert_eq!(
                "abcd [91111,22,3;44,55555,666]",
                app.editor_content.get_content()
            );
        }
        {
            let mut app = NoteCalcApp::new(120);
            app.handle_input(
                EditorInputEvent::Text("abcd [1111,22,3;44,55555,666]".to_owned()),
                InputModifiers::none(),
            );
            app.render();
            app.handle_input(EditorInputEvent::Left, InputModifiers::none());
            // inside the matrix
            app.handle_input(EditorInputEvent::Home, InputModifiers::none());
            app.render();
            app.handle_input(EditorInputEvent::Home, InputModifiers::none());
            app.render();
            app.handle_input(EditorInputEvent::Char('6'), InputModifiers::none());
            app.render();
            assert_eq!(
                "6abcd [1111,22,3;44,55555,666]",
                app.editor_content.get_content()
            );
        }
    }

    #[test]
    fn bug8() {
        let mut app = NoteCalcApp::new(120);
        app.handle_input(
            EditorInputEvent::Text("16892313\n14 * ".to_owned()),
            InputModifiers::none(),
        );
        app.editor
            .set_selection_save_col(Selection::single_r_c(1, 5));
        app.render();
        app.handle_input(EditorInputEvent::Up, InputModifiers::alt());
        app.alt_key_released();
        assert_eq!("16892313\n14 * &[1]", app.editor_content.get_content());
        app.render();
        app.handle_time(1000);
        app.handle_input(EditorInputEvent::Backspace, InputModifiers::none());
        assert_eq!("16892313\n14 * ", app.editor_content.get_content());

        app.handle_input(EditorInputEvent::Char('z'), InputModifiers::ctrl());
        assert_eq!("16892313\n14 * &[1]", app.editor_content.get_content());

        app.handle_input(EditorInputEvent::Right, InputModifiers::none()); // end selection
        app.render();
        app.handle_input(EditorInputEvent::Left, InputModifiers::none());
        app.handle_input(EditorInputEvent::Char('a'), InputModifiers::none());
        assert_eq!("16892313\n14 * a&[1]", app.editor_content.get_content());

        app.handle_input(EditorInputEvent::Char(' '), InputModifiers::none());
        app.render();
        app.handle_input(EditorInputEvent::Right, InputModifiers::none());
        app.handle_input(EditorInputEvent::Char('b'), InputModifiers::none());
        assert_eq!("16892313\n14 * a &[1]b", app.editor_content.get_content());

        app.handle_input(EditorInputEvent::Left, InputModifiers::none());
        app.handle_input(EditorInputEvent::Left, InputModifiers::none());
        app.handle_input(EditorInputEvent::Left, InputModifiers::none());
        app.handle_input(EditorInputEvent::Right, InputModifiers::none());
        app.handle_input(EditorInputEvent::Char('c'), InputModifiers::none());
        assert_eq!("16892313\n14 * a c&[1]b", app.editor_content.get_content());
    }

    #[test]
    fn test_line_ref_normalization() {
        let mut app = NoteCalcApp::new(120);
        app.handle_input(
            EditorInputEvent::Text("1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n".to_owned()),
            InputModifiers::none(),
        );
        app.editor
            .set_selection_save_col(Selection::single_r_c(12, 2));
        app.render();
        app.handle_input(EditorInputEvent::Up, InputModifiers::alt());
        app.alt_key_released();
        app.handle_input(EditorInputEvent::Up, InputModifiers::alt());
        app.alt_key_released();
        app.handle_input(EditorInputEvent::Up, InputModifiers::alt());
        app.alt_key_released();
        assert_eq!(
            "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13&[1]&[1]&[1]\n",
            &app.editor_content.get_content()
        );
        assert_eq!(
            "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13&[12]&[12]&[12]\n\n",
            &app.get_normalized_content()
        );
    }

    #[test]
    fn test_line_ref_denormalization() {
        let mut app = NoteCalcApp::new(120);
        app.set_normalized_content("1111\n2222\n14 * &[2]&[2]&[2]\n");
        assert_eq!(1, app.editor_content.get_data(0).line_id);
        assert_eq!(2, app.editor_content.get_data(1).line_id);
        assert_eq!(3, app.editor_content.get_data(2).line_id);
    }

    #[test]
    fn no_memory_deallocation_bug_in_line_selection() {
        let mut app = NoteCalcApp::new(120);
        app.handle_input(
            EditorInputEvent::Text("1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n".to_owned()),
            InputModifiers::none(),
        );
        app.editor
            .set_selection_save_col(Selection::single_r_c(12, 2));
        app.render();
        app.handle_input(EditorInputEvent::Up, InputModifiers::shift());
        app.render();
    }

    #[test]
    fn matrix_deletion() {
        let mut app = NoteCalcApp::new(120);
        app.handle_input(
            EditorInputEvent::Text(" [1,2,3]".to_owned()),
            InputModifiers::none(),
        );
        app.editor
            .set_selection_save_col(Selection::single_r_c(0, 0));
        app.render();
        app.handle_input(EditorInputEvent::Del, InputModifiers::none());
        assert_eq!("[1,2,3]", app.editor_content.get_content());
    }

    #[test]
    fn matrix_insertion_bug() {
        let mut app = NoteCalcApp::new(120);
        app.handle_input(
            EditorInputEvent::Text("[1,2,3]".to_owned()),
            InputModifiers::none(),
        );
        app.editor
            .set_selection_save_col(Selection::single_r_c(0, 0));
        app.render();
        app.handle_input(EditorInputEvent::Char('a'), InputModifiers::none());
        assert_eq!("a[1,2,3]", app.editor_content.get_content());
        app.handle_input(EditorInputEvent::Enter, InputModifiers::none());
        assert_eq!("a\n[1,2,3]", app.editor_content.get_content());
    }

    #[test]
    fn matrix_insertion_bug2() {
        let mut app = NoteCalcApp::new(120);
        app.handle_input(
            EditorInputEvent::Text("'[X] nth, sum fv".to_owned()),
            InputModifiers::none(),
        );
        app.render();
        app.editor
            .set_selection_save_col(Selection::single_r_c(0, 0));
        app.handle_input(EditorInputEvent::Del, InputModifiers::none());
        app.render();
        assert_results(app, &["0"][..]);
    }

    fn assert_results(app: NoteCalcApp, expected_results: &[&str]) {
        let mut i = 0;
        let mut ok_chars = Vec::with_capacity(32);
        let expected_len = expected_results.iter().map(|it| it.len()).sum();
        for r in expected_results.iter() {
            for ch in r.bytes() {
                assert_eq!(
                    app.result_buffer[i] as char,
                    ch as char,
                    "at {}: {:?}, result_buffer: {:?}",
                    i,
                    String::from_utf8(ok_chars).unwrap(),
                    &app.result_buffer[0..expected_len]
                        .iter()
                        .map(|it| *it as char)
                        .collect::<Vec<char>>()
                );
                ok_chars.push(ch);
                i += 1;
            }
            ok_chars.push(',' as u8);
            ok_chars.push(' ' as u8);
        }
        assert_eq!(app.result_buffer[i], 0, "more results than expected",);
    }

    #[test]
    fn sum_can_be_nullified() {
        let mut app = NoteCalcApp::new(120);
        app.handle_input(
            EditorInputEvent::Text(
                "3m * 2m
--
1
2
sum"
                .to_owned(),
            ),
            InputModifiers::none(),
        );
        app.render();
        assert_results(app, &["6 m^2", "", "1", "2", "3"][..]);
    }

    #[test]
    fn no_sum_value_in_case_of_error() {
        let mut app = NoteCalcApp::new(120);
        app.handle_input(
            EditorInputEvent::Text(
                "3m * 2m\n\
                4\n\
                sum"
                .to_owned(),
            ),
            InputModifiers::none(),
        );
        app.render();
        assert_results(app, &["6 m^2", "4", "0"][..]);
    }

    #[test]
    fn test_ctrl_c() {
        let mut app = NoteCalcApp::new(120);
        app.handle_input(
            EditorInputEvent::Text("aaaaaaaaa".to_owned()),
            InputModifiers::none(),
        );
        app.render();
        app.handle_input(EditorInputEvent::Left, InputModifiers::shift());
        app.handle_input(EditorInputEvent::Left, InputModifiers::shift());
        app.handle_input(EditorInputEvent::Left, InputModifiers::shift());
        app.handle_input(EditorInputEvent::Char('c'), InputModifiers::ctrl());
        assert_eq!("aaa", &app.editor.clipboard);
    }

    #[test]
    fn test_changing_output_style_for_selected_rows() {
        let mut app = NoteCalcApp::new(120);
        app.handle_input(
            EditorInputEvent::Text(
                "2\n\
                4\n\
                5"
                .to_owned(),
            ),
            InputModifiers::none(),
        );
        app.render();
        app.handle_input(EditorInputEvent::Up, InputModifiers::shift());
        app.handle_input(EditorInputEvent::Up, InputModifiers::shift());
        app.handle_input(EditorInputEvent::Left, InputModifiers::alt());
        app.render();
        assert_results(app, &["10", "100", "101"][..]);
    }

    #[test]
    fn test_matrix_sum() {
        let mut app = NoteCalcApp::new(120);
        app.handle_input(
            EditorInputEvent::Text("[1,2,3]\nsum".to_owned()),
            InputModifiers::none(),
        );
        app.render();
        // both the first line and the 'sum' line renders a matrix, which leaves the result bufer empty
        assert_results(app, &["\u{0}"][..]);
    }
}
