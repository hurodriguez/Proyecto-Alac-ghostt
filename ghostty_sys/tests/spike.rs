use ghostty_sys::*;
use std::ffi::c_void;
use std::ptr::null;

#[test]
fn feed_and_read_cells() {
    unsafe {
        let mut term: GhosttyTerminal = std::ptr::null_mut();
        assert_eq!(ghostty_terminal_new(null(), &mut term, 20, 5), GHOSTTY_SUCCESS);
        let msg = b"hi \x1b[31mred\x1b[0m\r\nline2";
        ghostty_terminal_vt_write(term, msg.as_ptr(), msg.len());

        let mut rs: GhosttyRenderState = std::ptr::null_mut();
        assert_eq!(ghostty_render_state_new(null(), &mut rs), GHOSTTY_SUCCESS);
        assert_eq!(ghostty_render_state_update(rs, term), GHOSTTY_SUCCESS);

        let mut it: GhosttyRenderStateRowIterator = std::ptr::null_mut();
        ghostty_render_state_row_iterator_new(null(), &mut it);
        ghostty_render_state_get(rs, GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR, &mut it as *mut _ as *mut c_void);
        let mut cells: GhosttyRenderStateRowCells = std::ptr::null_mut();
        ghostty_render_state_row_cells_new(null(), &mut cells);

        let mut out = Vec::new();
        while ghostty_render_state_row_iterator_next(it) {
            ghostty_render_state_row_get(it, GHOSTTY_RENDER_STATE_ROW_DATA_CELLS, &mut cells as *mut _ as *mut c_void);
            let mut line = String::new();
            while ghostty_render_state_row_cells_next(cells) {
                let mut n: u32 = 0;
                ghostty_render_state_row_cells_get(cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN, &mut n as *mut _ as *mut c_void);
                if n == 0 { line.push(' '); continue; }
                let mut buf = vec![0u32; n as usize];
                ghostty_render_state_row_cells_get(cells, GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF, buf.as_mut_ptr() as *mut c_void);
                for c in buf { line.push(char::from_u32(c).unwrap()); }
            }
            out.push(line);
        }
        println!("{out:#?}");
        assert!(out[0].starts_with("hi red"));
        assert!(out[1].starts_with("line2"));
    }
}
