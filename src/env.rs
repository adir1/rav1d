use crate::include::common::intops::apply_sign;
use crate::include::dav1d::headers::Dav1dFrameHeader;
use crate::include::dav1d::headers::Dav1dWarpedMotionParams;
use crate::include::stdint::int8_t;
use crate::include::stdint::uint8_t;
use crate::src::levels::mv;
use crate::src::levels::TxfmType;
use crate::src::levels::TxfmSize;
use crate::src::levels::DCT_DCT;
use crate::src::levels::H_ADST;
use crate::src::levels::H_FLIPADST;
use crate::src::levels::IDTX;
use crate::src::levels::TX_16X16;
use crate::src::levels::TX_32X32;
use crate::src::levels::V_ADST;
use crate::src::levels::V_FLIPADST;
use crate::src::tables::TxfmInfo;

#[derive(Copy, Clone)]
#[repr(C)]
pub struct BlockContext {
    pub mode: [uint8_t; 32],
    pub lcoef: [uint8_t; 32],
    pub ccoef: [[uint8_t; 32]; 2],
    pub seg_pred: [uint8_t; 32],
    pub skip: [uint8_t; 32],
    pub skip_mode: [uint8_t; 32],
    pub intra: [uint8_t; 32],
    pub comp_type: [uint8_t; 32],
    pub r#ref: [[int8_t; 32]; 2],
    pub filter: [[uint8_t; 32]; 2],
    pub tx_intra: [int8_t; 32],
    pub tx: [int8_t; 32],
    pub tx_lpf_y: [uint8_t; 32],
    pub tx_lpf_uv: [uint8_t; 32],
    pub partition: [uint8_t; 16],
    pub uvmode: [uint8_t; 32],
    pub pal_sz: [uint8_t; 32],
}

#[inline]
pub fn get_uv_inter_txtp(
    uvt_dim: &TxfmInfo,
    ytxtp: TxfmType,
) -> TxfmType {
    if (*uvt_dim).max as TxfmSize == TX_32X32 {
        return if ytxtp == IDTX {
            IDTX
        } else {
            DCT_DCT
        };
    }
    if (*uvt_dim).min as TxfmSize == TX_16X16
        && ((1 << ytxtp) & ((1 << H_FLIPADST) | (1 << V_FLIPADST) | (1 << H_ADST) | (1 << V_ADST))) != 0 {
        return DCT_DCT;
    }

    return ytxtp;
}

#[inline]
pub unsafe extern "C" fn get_poc_diff(
    order_hint_n_bits: libc::c_int,
    poc0: libc::c_int,
    poc1: libc::c_int,
) -> libc::c_int {
    if order_hint_n_bits == 0 {
        return 0 as libc::c_int;
    }
    let mask: libc::c_int = (1 as libc::c_int) << order_hint_n_bits - 1 as libc::c_int;
    let diff: libc::c_int = poc0 - poc1;
    return (diff & mask - 1 as libc::c_int) - (diff & mask);
}

#[inline]
fn fix_int_mv_precision(mv: &mut mv) {
    mv.x = (mv.x - (mv.x >> 15) + 3) & !7;
    mv.y = (mv.y - (mv.y >> 15) + 3) & !7;
}

#[inline]
pub fn fix_mv_precision(hdr: &Dav1dFrameHeader, mv: &mut mv) {
    if hdr.force_integer_mv != 0 {
        fix_int_mv_precision(mv);
    } else if (*hdr).hp == 0 {
        mv.x = (mv.x - (mv.x >> 15)) & !1;
        mv.y = (mv.y - (mv.y >> 15)) & !1;
    }
}

#[inline]
pub fn get_gmv_2d(
    gmv: &Dav1dWarpedMotionParams,
    bx4: libc::c_int,
    by4: libc::c_int,
    bw4: libc::c_int,
    bh4: libc::c_int,
    hdr: &Dav1dFrameHeader,
) -> mv {
    match gmv.type_0 {
        2 => {
            assert!(gmv.matrix[5] == gmv.matrix[2]);
            assert!(gmv.matrix[4] == -gmv.matrix[3]);
        }
        1 => {
            let mut res = mv {
                y: (gmv.matrix[0] >> 13) as i16,
                x: (gmv.matrix[1] >> 13) as i16,
            };
            if hdr.force_integer_mv != 0 {
                fix_int_mv_precision(&mut res);
            }
            return res;
        }
        0 => {
            return mv::ZERO;
        }
        3 | _ => {}
    }
    let x = bx4 * 4 + bw4 * 2 - 1;
    let y = by4 * 4 + bh4 * 2 - 1;
    let xc = (gmv.matrix[2] - (1 << 16)) * x + gmv.matrix[3] * y + gmv.matrix[0];
    let yc = (gmv.matrix[5] - (1 << 16)) * y + gmv.matrix[4] * x + gmv.matrix[1];
    let shift = 16 - (3 - (hdr.hp == 0) as libc::c_int);
    let round = 1 << shift >> 1;
    let mut res = mv {
        y: apply_sign(yc.abs() + round >> shift << (hdr.hp == 0) as libc::c_int, yc) as i16,
        x: apply_sign(xc.abs() + round >> shift << (hdr.hp == 0) as libc::c_int, xc) as i16,
    };
    if hdr.force_integer_mv != 0 {
        fix_int_mv_precision(&mut res);
    }
    return res;
}
