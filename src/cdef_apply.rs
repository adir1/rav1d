use crate::include::common::bitdepth::BitDepth;
use crate::include::common::bitdepth::BPC;
use crate::include::common::intops::ulog2;
use crate::include::dav1d::headers::Rav1dPixelLayout;
use crate::include::dav1d::picture::Rav1dPictureDataComponentOffset;
use crate::src::align::Align16;
use crate::src::align::AlignedVec64;
use crate::src::cdef::CdefEdgeFlags;
use crate::src::disjoint_mut::DisjointMut;
use crate::src::internal::Rav1dContext;
use crate::src::internal::Rav1dFrameData;
use crate::src::internal::Rav1dTaskContext;
use bitflags::bitflags;
use libc::ptrdiff_t;
use std::cmp;
use std::ffi::c_int;
use std::ffi::c_uint;
use std::sync::atomic::Ordering;

bitflags! {
    #[derive(Clone, Copy)]
    struct Backup2x8Flags: u8 {
        const Y = 1 << 0;
        const UV = 1 << 1;
    }
}

impl Backup2x8Flags {
    pub const fn select(&self, select: bool) -> Self {
        if select {
            *self
        } else {
            Self::empty()
        }
    }
}

/// `dst_buf` is a buffer of [`BitDepth::Pixel`] elements.
fn backup2lines<BD: BitDepth>(
    dst_buf: &DisjointMut<AlignedVec64<u8>>,
    dst_off: [usize; 3],
    src: [Rav1dPictureDataComponentOffset; 3],
    layout: Rav1dPixelLayout,
) {
    let y_stride = src[0].data.pixel_stride::<BD>();
    let y_len = 2 * y_stride.unsigned_abs();
    let y_strides = if y_stride < 0 { 1 } else { 0 };
    let y_src = src[0] + (6 + y_strides) * y_stride;
    let y_dst_offset = dst_off[0].wrapping_add_signed(y_strides * y_stride);
    BD::pixel_copy(
        &mut dst_buf.mut_slice_as((y_dst_offset.., ..y_len)),
        &y_src.data.slice::<BD, _>((y_src.offset.., ..y_len)),
        y_len,
    );

    if layout == Rav1dPixelLayout::I400 {
        return;
    }

    for pl in 1..3 {
        let uv_stride = src[pl].data.pixel_stride::<BD>();
        let uv_len = 2 * uv_stride.unsigned_abs();
        let uv_strides = if uv_stride < 0 { 1 } else { 0 };
        let uv_src_strides = match layout {
            Rav1dPixelLayout::I420 => 2,
            _ => 6,
        };
        let uv_src = src[pl] + (uv_src_strides + uv_strides) * uv_stride;
        let uv_dst_offset = dst_off[pl].wrapping_add_signed(uv_strides * uv_stride);
        BD::pixel_copy(
            &mut dst_buf.mut_slice_as((uv_dst_offset.., ..uv_len)),
            &uv_src.data.slice::<BD, _>((uv_src.offset.., ..uv_len)),
            uv_len,
        );
    }
}

fn backup2x8<BD: BitDepth>(
    dst: &mut [[[BD::Pixel; 2]; 8]; 3],
    src: [Rav1dPictureDataComponentOffset; 3],
    x_off: c_int,
    layout: Rav1dPixelLayout,
    flag: Backup2x8Flags,
) {
    let x_off = x_off as isize;

    if flag.contains(Backup2x8Flags::Y) {
        for y in 0..8 {
            let y_dst = &mut dst[0][y];
            let y_src = src[0] + (y as isize * src[0].data.pixel_stride::<BD>() + x_off - 2);
            let y_len = y_dst.len();
            BD::pixel_copy(
                y_dst,
                &y_src.data.slice::<BD, _>((y_src.offset.., ..y_len)),
                y_len,
            );
        }
    }

    if layout == Rav1dPixelLayout::I400 || !flag.contains(Backup2x8Flags::UV) {
        return;
    }

    let ss_ver = (layout == Rav1dPixelLayout::I420) as c_int;
    let ss_hor = (layout != Rav1dPixelLayout::I444) as c_int;

    let x_off = x_off >> ss_hor;
    for y in 0..8 >> ss_ver {
        for pl in 1..3 {
            let uv_dst = &mut dst[pl][y];
            let uv_src = src[pl] + (y as isize * src[pl].data.pixel_stride::<BD>() + x_off - 2);
            let uv_len = uv_dst.len();
            BD::pixel_copy(
                uv_dst,
                &uv_src.data.slice::<BD, _>((uv_src.offset.., ..uv_len)),
                uv_len,
            );
        }
    }
}

fn adjust_strength(strength: u8, var: c_uint) -> c_int {
    if var == 0 {
        return 0;
    }

    let i = if var >> 6 != 0 {
        cmp::min(ulog2(var >> 6), 12 as c_int)
    } else {
        0
    };

    strength as c_int * (4 + i) + 8 >> 4
}

pub(crate) unsafe fn rav1d_cdef_brow<BD: BitDepth>(
    c: &Rav1dContext,
    tc: &mut Rav1dTaskContext,
    f: &Rav1dFrameData,
    p: [Rav1dPictureDataComponentOffset; 3],
    lflvl_offset: i32,
    by_start: c_int,
    by_end: c_int,
    sbrow_start: bool,
    sby: c_int,
) {
    let bd = BD::from_c(f.bitdepth_max);

    let bitdepth_min_8 = match BD::BPC {
        BPC::BPC8 => 0,
        BPC::BPC16 => f.cur.p.bpc - 8,
    };
    let mut edges: CdefEdgeFlags = if by_start > 0 {
        CdefEdgeFlags::HAVE_BOTTOM | CdefEdgeFlags::HAVE_TOP
    } else {
        CdefEdgeFlags::HAVE_BOTTOM
    };
    let mut ptrs = p;
    let sbsz = 16;
    let sb64w = f.sb128w << 1;
    let frame_hdr = &***f.frame_hdr.as_ref().unwrap();
    let damping = frame_hdr.cdef.damping + bitdepth_min_8;
    let layout: Rav1dPixelLayout = f.cur.p.layout;
    let uv_idx = (Rav1dPixelLayout::I444 as c_uint).wrapping_sub(layout as c_uint) as c_int;
    let ss_ver = (layout == Rav1dPixelLayout::I420) as c_int;
    let ss_hor = (layout != Rav1dPixelLayout::I444) as c_int;

    static UV_DIRS: [[u8; 8]; 2] = [[0, 1, 2, 3, 4, 5, 6, 7], [7, 0, 2, 4, 5, 6, 6, 6]];
    let uv_dir: &[u8; 8] = &UV_DIRS[(layout == Rav1dPixelLayout::I422) as usize];

    let have_tt = c.tc.len() > 1;
    let sb128 = f.seq_hdr.as_ref().unwrap().sb128;
    let resize = frame_hdr.size.width[0] != frame_hdr.size.width[1];
    let y_stride: ptrdiff_t = BD::pxstride(f.cur.stride[0]);
    let uv_stride: ptrdiff_t = BD::pxstride(f.cur.stride[1]);

    let mut bit = false;
    for by in (by_start..by_end).step_by(2) {
        let tf = tc.top_pre_cdef_toggle != 0;
        let by_idx = (by & 30) >> 1;
        if by + 2 >= f.bh {
            edges.remove(CdefEdgeFlags::HAVE_BOTTOM);
        }

        if (!have_tt || sbrow_start || (by + 2) < by_end)
            && edges.contains(CdefEdgeFlags::HAVE_BOTTOM)
        {
            // backup pre-filter data for next iteration
            let cdef_top_bak = [
                f.lf.cdef_line[!tf as usize][0]
                    .wrapping_add_signed(have_tt as isize * sby as isize * 4 * y_stride),
                f.lf.cdef_line[!tf as usize][1]
                    .wrapping_add_signed(have_tt as isize * sby as isize * 8 * uv_stride),
                f.lf.cdef_line[!tf as usize][2]
                    .wrapping_add_signed(have_tt as isize * sby as isize * 8 * uv_stride),
            ];
            backup2lines::<BD>(&f.lf.cdef_line_buf, cdef_top_bak, ptrs, layout);
        }

        let mut lr_bak =
            Align16([[[[0.into(); 2 /* x */]; 8 /* y */]; 3 /* plane */ ]; 2 /* idx */]);
        let mut iptrs = ptrs;
        edges.remove(CdefEdgeFlags::HAVE_LEFT);
        edges.insert(CdefEdgeFlags::HAVE_RIGHT);
        let mut prev_flag = Backup2x8Flags::empty();
        let mut last_skip = true;
        for sbx in 0..sb64w {
            let sb128x = sbx >> 1;
            let sb64_idx = ((by & sbsz) >> 3) + (sbx & 1);
            let cdef_idx = f.lf.mask[(lflvl_offset + sb128x) as usize].cdef_idx[sb64_idx as usize]
                .load(atomig::Ordering::Relaxed) as c_int;
            if cdef_idx == -1
                || frame_hdr.cdef.y_strength[cdef_idx as usize] == 0
                    && frame_hdr.cdef.uv_strength[cdef_idx as usize] == 0
            {
                last_skip = true;
            } else {
                // Create a complete 32-bit mask for the sb row ahead of time.
                let noskip_row =
                    &f.lf.mask[(lflvl_offset + sb128x) as usize].noskip_mask[by_idx as usize];
                let noskip_mask = (noskip_row[1].load(Ordering::Relaxed) as u32) << 16
                    | noskip_row[0].load(Ordering::Relaxed) as u32;

                let y_lvl = frame_hdr.cdef.y_strength[cdef_idx as usize];
                let uv_lvl = frame_hdr.cdef.uv_strength[cdef_idx as usize];
                let flag =
                    Backup2x8Flags::Y.select(y_lvl != 0) | Backup2x8Flags::UV.select(uv_lvl != 0);

                let y_pri_lvl = (y_lvl >> 2) << bitdepth_min_8;
                let mut y_sec_lvl = y_lvl & 3;
                y_sec_lvl += (y_sec_lvl == 3) as u8;
                y_sec_lvl <<= bitdepth_min_8;

                let uv_pri_lvl = (uv_lvl >> 2) << bitdepth_min_8;
                let mut uv_sec_lvl = uv_lvl & 3;
                uv_sec_lvl += (uv_sec_lvl == 3) as u8;
                uv_sec_lvl <<= bitdepth_min_8;

                let mut bptrs = iptrs;
                for bx in (sbx * sbsz..cmp::min((sbx + 1) * sbsz, f.bw)).step_by(2) {
                    if bx + 2 >= f.bw {
                        edges.remove(CdefEdgeFlags::HAVE_RIGHT);
                    }

                    // check if this 8x8 block had any coded coefficients; if not, go to the next block
                    let bx_mask: u32 = 3 << (bx & 30);
                    if noskip_mask & bx_mask == 0 {
                        last_skip = true;
                    } else {
                        let do_left = if last_skip {
                            flag
                        } else {
                            (prev_flag ^ flag) & flag
                        };
                        prev_flag = flag;
                        if !do_left.is_empty() && edges.contains(CdefEdgeFlags::HAVE_LEFT) {
                            // we didn't backup the prefilter data because it wasn't
                            // there, so do it here instead
                            backup2x8::<BD>(&mut lr_bak[bit as usize], bptrs, 0, layout, do_left);
                        }
                        if edges.contains(CdefEdgeFlags::HAVE_RIGHT) {
                            // backup pre-filter data for next iteration
                            backup2x8::<BD>(&mut lr_bak[!bit as usize], bptrs, 8, layout, flag);
                        }

                        let mut variance = 0;
                        let dir = if y_pri_lvl != 0 || uv_pri_lvl != 0 {
                            f.dsp.cdef.dir.call::<BD>(bptrs[0], &mut variance, bd)
                        } else {
                            0
                        };

                        let mut top = 0 as *const BD::Pixel;
                        let mut bot = 0 as *const BD::Pixel;
                        let mut offset: ptrdiff_t;
                        let st_y: bool;

                        if !have_tt {
                            st_y = true;
                        } else if sbrow_start && by == by_start {
                            if resize {
                                offset = ((sby - 1) * 4) as isize * y_stride + (bx * 4) as isize;
                                top = &*f
                                    .lf
                                    .cdef_line_buf
                                    .element_as((f.lf.cdef_lpf_line[0] as isize + offset) as usize);
                            } else {
                                offset = (sby * ((4 as c_int) << sb128) - 4) as isize * y_stride
                                    + (bx * 4) as isize;
                                top = &*f
                                    .lf
                                    .lr_line_buf
                                    .element_as((f.lf.lr_lpf_line[0] as isize + offset) as usize);
                            }
                            let bottom = bptrs[0] + (8 * y_stride);
                            bot = bottom.data.as_ptr_at::<BD>(bottom.offset);
                            st_y = false;
                        } else if !sbrow_start && by + 2 >= by_end {
                            offset = (sby * 4) as isize * y_stride + (bx * 4) as isize;
                            top = &*f.lf.cdef_line_buf.element_as(
                                (f.lf.cdef_line[tf as usize][0] as isize + offset) as usize,
                            );
                            if resize {
                                offset = (sby * 4 + 2) as isize * y_stride + (bx * 4) as isize;
                                // FIXME incorrect; should be kept as an offset for later slices.
                                bot = &*f
                                    .lf
                                    .cdef_line_buf
                                    .element_as((f.lf.cdef_lpf_line[0] as isize + offset) as usize);
                            } else {
                                let line = sby * ((4 as c_int) << sb128) + 4 * sb128 as c_int + 2;
                                offset = line as isize * y_stride + (bx * 4) as isize;
                                // FIXME incorrect; should be kept as an offset for later slices.
                                bot = &*f
                                    .lf
                                    .lr_line_buf
                                    .element_as((f.lf.lr_lpf_line[0] as isize + offset) as usize);
                            }
                            st_y = false;
                        } else {
                            st_y = true;
                        }

                        if st_y {
                            offset = have_tt as isize * (sby * 4) as isize * y_stride
                                + (bx * 4) as isize;
                            top = &*f.lf.cdef_line_buf.element_as(
                                (f.lf.cdef_line[tf as usize][0] as isize + offset) as usize,
                            );
                            let bottom = bptrs[0] + (8 * y_stride);
                            bot = bottom.data.as_ptr_at::<BD>(bottom.offset);
                        }

                        if y_pri_lvl != 0 {
                            let adj_y_pri_lvl = adjust_strength(y_pri_lvl, variance);
                            if adj_y_pri_lvl != 0 || y_sec_lvl != 0 {
                                f.dsp.cdef.fb[0].call::<BD>(
                                    bptrs[0],
                                    &lr_bak[bit as usize][0],
                                    top,
                                    bot,
                                    adj_y_pri_lvl,
                                    y_sec_lvl,
                                    dir,
                                    damping,
                                    edges,
                                    bd,
                                );
                            }
                        } else if y_sec_lvl != 0 {
                            f.dsp.cdef.fb[0].call::<BD>(
                                bptrs[0],
                                &lr_bak[bit as usize][0],
                                top,
                                bot,
                                0,
                                y_sec_lvl,
                                0,
                                damping,
                                edges,
                                bd,
                            );
                        }
                        if uv_lvl != 0 {
                            if !(layout != Rav1dPixelLayout::I400) {
                                unreachable!();
                            }
                            let uvdir = if uv_pri_lvl != 0 {
                                uv_dir[dir as usize] as c_int
                            } else {
                                0
                            };
                            for pl in 1..=2 {
                                let st_uv: bool;
                                if !have_tt {
                                    st_uv = true;
                                } else if sbrow_start && by == by_start {
                                    if resize {
                                        offset = ((sby - 1) * 4) as isize * uv_stride
                                            + (bx * 4 >> ss_hor) as isize;
                                        top = &*f.lf.cdef_line_buf.element_as(
                                            (f.lf.cdef_lpf_line[pl] as isize + offset) as usize,
                                        );
                                    } else {
                                        let line_0 = sby * ((4 as c_int) << sb128) - 4;
                                        offset = line_0 as isize * uv_stride
                                            + (bx * 4 >> ss_hor) as isize;
                                        top = &*f.lf.lr_line_buf.element_as(
                                            (f.lf.lr_lpf_line[pl] as isize + offset) as usize,
                                        );
                                    }
                                    let bottom = bptrs[pl] + ((8 >> ss_ver) * uv_stride);
                                    bot = bottom.data.as_ptr_at::<BD>(bottom.offset);
                                    st_uv = false;
                                } else if !sbrow_start && by + 2 >= by_end {
                                    let top_offset: ptrdiff_t = (sby * 8) as isize * uv_stride
                                        + (bx * 4 >> ss_hor) as isize;
                                    top = &*f.lf.cdef_line_buf.element_as(
                                        (f.lf.cdef_line[tf as usize][pl] as isize + top_offset)
                                            as usize,
                                    );
                                    if resize {
                                        offset = (sby * 4 + 2) as isize * uv_stride
                                            + (bx * 4 >> ss_hor) as isize;
                                        // FIXME incorrect; should be kept as an offset for later slices.
                                        bot = &*f.lf.cdef_line_buf.element_as(
                                            (f.lf.cdef_lpf_line[pl] as isize + offset) as usize,
                                        );
                                    } else {
                                        let line =
                                            sby * ((4 as c_int) << sb128) + 4 * sb128 as c_int + 2;
                                        offset =
                                            line as isize * uv_stride + (bx * 4 >> ss_hor) as isize;
                                        // FIXME incorrect; should be kept as an offset for later slices.
                                        bot = &*f.lf.lr_line_buf.element_as(
                                            (f.lf.lr_lpf_line[pl] as isize + offset) as usize,
                                        );
                                    }
                                    st_uv = false;
                                } else {
                                    st_uv = true;
                                }

                                if st_uv {
                                    let offset = have_tt as isize * (sby * 8) as isize * uv_stride
                                        + (bx * 4 >> ss_hor) as isize;
                                    top = &*f.lf.cdef_line_buf.element_as(
                                        (f.lf.cdef_line[tf as usize][pl] as isize + offset)
                                            as usize,
                                    );
                                    let bottom = bptrs[pl] + ((8 >> ss_ver) * uv_stride);
                                    bot = bottom.data.as_ptr_at::<BD>(bottom.offset);
                                }

                                f.dsp.cdef.fb[uv_idx as usize].call::<BD>(
                                    bptrs[pl],
                                    &lr_bak[bit as usize][pl],
                                    top,
                                    bot,
                                    uv_pri_lvl.into(),
                                    uv_sec_lvl,
                                    uvdir,
                                    damping - 1,
                                    edges,
                                    bd,
                                );
                            }
                        }
                        bit = !bit;
                        last_skip = false;
                    }
                    bptrs[0] += 8usize;
                    bptrs[1] += 8usize >> ss_hor;
                    bptrs[2] += 8usize >> ss_hor;
                    edges.insert(CdefEdgeFlags::HAVE_LEFT);
                }
            }
            iptrs[0] += sbsz as usize * 4;
            iptrs[1] += sbsz as usize * 4 >> ss_hor;
            iptrs[2] += sbsz as usize * 4 >> ss_hor;
            edges.insert(CdefEdgeFlags::HAVE_LEFT);
        }
        ptrs[0] += 8 * ptrs[0].data.pixel_stride::<BD>();
        ptrs[1] += 8 * (ptrs[1].data.pixel_stride::<BD>() >> ss_ver);
        ptrs[2] += 8 * (ptrs[2].data.pixel_stride::<BD>() >> ss_ver);
        tc.top_pre_cdef_toggle ^= 1 as c_int;
        edges.insert(CdefEdgeFlags::HAVE_TOP);
    }
}
