/*
 * Copyright (c) 2024 Intel Corporation. All Rights Reserved.
 *
 * Permission is hereby granted, free of charge, to any person obtaining a
 * copy of this software and associated documentation files (the
 * "Software"), to deal in the Software without restriction, including
 * without limitation the rights to use, copy, modify, merge, publish,
 * distribute, sub license, and/or sell copies of the Software, and to
 * permit persons to whom the Software is furnished to do so, subject to
 * the following conditions:
 *
 * The above copyright notice and this permission notice (including the
 * next paragraph) shall be included in all copies or substantial portions
 * of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
 * OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
 * MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NON-INFRINGEMENT.
 * IN NO EVENT SHALL INTEL AND/OR ITS SUPPLIERS BE LIABLE FOR
 * ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT,
 * TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
 * SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
 */

/**
 * \file va_dec_vvc.h
 * \brief The VVC decoding API
 *
 * This file contains the \ref api_dec_vvc "VVC decoding API".
 */

#ifndef VA_DEC_VVC_H
#define VA_DEC_VVC_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/**
 * \defgroup api_dec_vvc VVC decoding API
 *
 * This VVC decoding API supports Main 10 profile and Multilayer Main 10 profile.
 * And it supports only long slice format.
 *
 * @{
 */

/**
 * \brief Weighted Prediction Parameters.
 */
typedef struct _VAWeightedPredInfo {
    /** \brief Weighted Prediction parameters.
     *  All the parameters except reserved bytes are VVC syntax.
     */
    uint8_t                 luma_log2_weight_denom;
    int8_t                  delta_chroma_log2_weight_denom;
    uint8_t                 num_l0_weights;
    uint8_t                 luma_weight_l0_flag[15];
    uint8_t                 chroma_weight_l0_flag[15];
    int8_t                  delta_luma_weight_l0[15];
    int8_t                  luma_offset_l0[15];
    int8_t                  delta_chroma_weight_l0[15][2];
    int16_t                 delta_chroma_offset_l0[15][2];
    uint8_t                 num_l1_weights;
    uint8_t                 luma_weight_l1_flag[15];
    uint8_t                 chroma_weight_l1_flag[15];
    int8_t                  delta_luma_weight_l1[15];
    int8_t                  luma_offset_l1[15];
    int8_t                  delta_chroma_weight_l1[15][2];
    int16_t                 delta_chroma_offset_l1[15][2];
    /** \brief Reserved for future use, must be zero */
    uint16_t                reserved16b;
    uint32_t                reserved32b;
} VAWeightedPredInfo;

/**
 * \brief VVC Decoding Picture Parameter Buffer Structure
 *
 * This structure conveys picture level parameters and should be sent once
 * per frame.
 *
 * Host decoder is required to send in a buffer of VAPictureParameterBufferVVC
 * as the first va buffer for each frame.
 *
 */
typedef struct  _VAPictureParameterBufferVVC {
    /** \brief buffer description of decoded current picture
     */
    VAPictureVVC            CurrPic;
    /** \brief buffer description of reference frames in DPB */
    VAPictureVVC            ReferenceFrames[15];
    /** \brief picture width, shall be integer multiple of Max(8, MinCbSizeY). */
    uint16_t                pps_pic_width_in_luma_samples;
    /** \brief picture height, shall be integer multiple of Max(8, MinCbSizeY). */
    uint16_t                pps_pic_height_in_luma_samples;

    /** \brief sequence level parameters.
     *  All the parameters except reserved bytes are VVC syntax or spec variables.
     */
    uint16_t                sps_num_subpics_minus1;
    uint8_t                 sps_chroma_format_idc;
    uint8_t                 sps_bitdepth_minus8;
    uint8_t                 sps_log2_ctu_size_minus5;
    uint8_t                 sps_log2_min_luma_coding_block_size_minus2;
    uint8_t                 sps_log2_transform_skip_max_size_minus2;
    /** \brief chroma QP mapping table.
     *  ChromaQpTable[][] corresponds to VVC spec variable with the same name.
     *  It is derived according to formula (57) in VVC spec section 7.4.3.4.
     */
    int8_t                  ChromaQpTable[3][111];
    uint8_t                 sps_six_minus_max_num_merge_cand;
    uint8_t                 sps_five_minus_max_num_subblock_merge_cand;
    uint8_t                 sps_max_num_merge_cand_minus_max_num_gpm_cand;
    uint8_t                 sps_log2_parallel_merge_level_minus2;
    uint8_t                 sps_min_qp_prime_ts;
    uint8_t                 sps_six_minus_max_num_ibc_merge_cand;
    uint8_t                 sps_num_ladf_intervals_minus2;
    int8_t                  sps_ladf_lowest_interval_qp_offset;
    int8_t                  sps_ladf_qp_offset[4];
    uint16_t                sps_ladf_delta_threshold_minus1[4];
    /** \brief Reserved for future use, must be zero */
    uint32_t                reserved32b01[VA_PADDING_LOW - 2];

    union {
        struct {
            uint64_t        sps_subpic_info_present_flag                                    : 1;
            uint64_t        sps_independent_subpics_flag                                    : 1;
            uint64_t        sps_subpic_same_size_flag                                       : 1;
            uint64_t        sps_entropy_coding_sync_enabled_flag                            : 1;
            uint64_t        sps_qtbtt_dual_tree_intra_flag                                  : 1;
            uint64_t        sps_max_luma_transform_size_64_flag                             : 1;
            uint64_t        sps_transform_skip_enabled_flag                                 : 1;
            uint64_t        sps_bdpcm_enabled_flag                                          : 1;
            uint64_t        sps_mts_enabled_flag                                            : 1;
            uint64_t        sps_explicit_mts_intra_enabled_flag                             : 1;
            uint64_t        sps_explicit_mts_inter_enabled_flag                             : 1;
            uint64_t        sps_lfnst_enabled_flag                                          : 1;
            uint64_t        sps_joint_cbcr_enabled_flag                                     : 1;
            uint64_t        sps_same_qp_table_for_chroma_flag                               : 1;
            uint64_t        sps_sao_enabled_flag                                            : 1;
            uint64_t        sps_alf_enabled_flag                                            : 1;
            uint64_t        sps_ccalf_enabled_flag                                          : 1;
            uint64_t        sps_lmcs_enabled_flag                                           : 1;
            uint64_t        sps_sbtmvp_enabled_flag                                         : 1;
            uint64_t        sps_amvr_enabled_flag                                           : 1;
            uint64_t        sps_smvd_enabled_flag                                           : 1;
            uint64_t        sps_mmvd_enabled_flag                                           : 1;
            uint64_t        sps_sbt_enabled_flag                                            : 1;
            uint64_t        sps_affine_enabled_flag                                         : 1;
            uint64_t        sps_6param_affine_enabled_flag                                  : 1;
            uint64_t        sps_affine_amvr_enabled_flag                                    : 1;
            uint64_t        sps_affine_prof_enabled_flag                                    : 1;
            uint64_t        sps_bcw_enabled_flag                                            : 1;
            uint64_t        sps_ciip_enabled_flag                                           : 1;
            uint64_t        sps_gpm_enabled_flag                                            : 1;
            uint64_t        sps_isp_enabled_flag                                            : 1;
            uint64_t        sps_mrl_enabled_flag                                            : 1;
            uint64_t        sps_mip_enabled_flag                                            : 1;
            uint64_t        sps_cclm_enabled_flag                                           : 1;
            uint64_t        sps_chroma_horizontal_collocated_flag                           : 1;
            uint64_t        sps_chroma_vertical_collocated_flag                             : 1;
            uint64_t        sps_palette_enabled_flag                                        : 1;
            uint64_t        sps_act_enabled_flag                                            : 1;
            uint64_t        sps_ibc_enabled_flag                                            : 1;
            uint64_t        sps_ladf_enabled_flag                                           : 1;
            uint64_t        sps_explicit_scaling_list_enabled_flag                          : 1;
            uint64_t        sps_scaling_matrix_for_lfnst_disabled_flag                      : 1;
            uint64_t        sps_scaling_matrix_for_alternative_colour_space_disabled_flag   : 1;
            uint64_t        sps_scaling_matrix_designated_colour_space_flag                 : 1;
            uint64_t        sps_virtual_boundaries_enabled_flag                             : 1;
            uint64_t        sps_virtual_boundaries_present_flag                             : 1;
            /** \brief Reserved for future use, must be zero */
            uint64_t        reserved                                                        : 18;
        } bits;
        uint64_t            value;
    } sps_flags;

    /** \brief picture level parameters.
     *  All the parameters except reserved bytes are VVC syntax or spec variables.
     */
    /** \brief number of vertical virtual boundaries on the picture.
     *  NumVerVirtualBoundaries corresponds to VVC spec variable with the same name.
     *  It is derived according to formula (78) in VVC spec section 7.4.3.8.
     */
    uint8_t                 NumVerVirtualBoundaries;
    /** \brief number of horizontal virtual boundaries on the picture.
     *  NumHorVirtualBoundaries corresponds to VVC spec variable with the same name.
     *  It is derived according to formula (80) in VVC spec section 7.4.3.8.
     */
    uint8_t                 NumHorVirtualBoundaries;
    /** \brief location of the vertical virtual boundary in units of luma samples.
     *  VirtualBoundaryPosX[] corresponds to VVC spec variable with the same name.
     *  It is derived according to formula (79) in VVC spec section 7.4.3.8.
     */
    uint16_t                VirtualBoundaryPosX[3];
    /** \brief location of the horizontal virtual boundary in units of luma samples.
     *  VirtualBoundaryPosY[] corresponds to VVC spec variable with the same name.
     *  It is derived according to formula (81) in VVC spec section 7.4.3.8.
     */
    uint16_t                VirtualBoundaryPosY[3];

    int32_t                 pps_scaling_win_left_offset;
    int32_t                 pps_scaling_win_right_offset;
    int32_t                 pps_scaling_win_top_offset;
    int32_t                 pps_scaling_win_bottom_offset;

    int8_t                  pps_num_exp_tile_columns_minus1;
    uint16_t                pps_num_exp_tile_rows_minus1;
    uint16_t                pps_num_slices_in_pic_minus1;
    uint16_t                pps_pic_width_minus_wraparound_offset;
    int8_t                  pps_cb_qp_offset;
    int8_t                  pps_cr_qp_offset;
    int8_t                  pps_joint_cbcr_qp_offset_value;
    uint8_t                 pps_chroma_qp_offset_list_len_minus1;
    int8_t                  pps_cb_qp_offset_list[6];
    int8_t                  pps_cr_qp_offset_list[6];
    int8_t                  pps_joint_cbcr_qp_offset_list[6];
    /** \brief Reserved for future use, must be zero */
    uint16_t                reserved16b01;
    uint32_t                reserved32b02[VA_PADDING_LOW - 2];

    union {
        struct {
            uint32_t        pps_loop_filter_across_tiles_enabled_flag                       : 1;
            uint32_t        pps_rect_slice_flag                                             : 1;
            uint32_t        pps_single_slice_per_subpic_flag                                : 1;
            uint32_t        pps_loop_filter_across_slices_enabled_flag                      : 1;
            uint32_t        pps_weighted_pred_flag                                          : 1;
            uint32_t        pps_weighted_bipred_flag                                        : 1;
            uint32_t        pps_ref_wraparound_enabled_flag                                 : 1;
            uint32_t        pps_cu_qp_delta_enabled_flag                                    : 1;
            uint32_t        pps_cu_chroma_qp_offset_list_enabled_flag                       : 1;
            uint32_t        pps_deblocking_filter_override_enabled_flag                     : 1;
            uint32_t        pps_deblocking_filter_disabled_flag                             : 1;
            uint32_t        pps_dbf_info_in_ph_flag                                         : 1;
            uint32_t        pps_sao_info_in_ph_flag                                         : 1;
            uint32_t        pps_alf_info_in_ph_flag                                         : 1;
            /** \brief Reserved for future use, must be zero */
            uint32_t        reserved                                                        : 18;
        } bits;
        uint32_t            value;
    } pps_flags;

    /** \brief picture header parameters.
     *  All the parameters except reserved bytes are VVC syntax or spec variables.
     */
    uint8_t                 ph_lmcs_aps_id;
    uint8_t                 ph_scaling_list_aps_id;
    uint8_t                 ph_log2_diff_min_qt_min_cb_intra_slice_luma;
    uint8_t                 ph_max_mtt_hierarchy_depth_intra_slice_luma;
    uint8_t                 ph_log2_diff_max_bt_min_qt_intra_slice_luma;
    uint8_t                 ph_log2_diff_max_tt_min_qt_intra_slice_luma;
    uint8_t                 ph_log2_diff_min_qt_min_cb_intra_slice_chroma;
    uint8_t                 ph_max_mtt_hierarchy_depth_intra_slice_chroma;
    uint8_t                 ph_log2_diff_max_bt_min_qt_intra_slice_chroma;
    uint8_t                 ph_log2_diff_max_tt_min_qt_intra_slice_chroma;
    uint8_t                 ph_cu_qp_delta_subdiv_intra_slice;
    uint8_t                 ph_cu_chroma_qp_offset_subdiv_intra_slice;
    uint8_t                 ph_log2_diff_min_qt_min_cb_inter_slice;
    uint8_t                 ph_max_mtt_hierarchy_depth_inter_slice;
    uint8_t                 ph_log2_diff_max_bt_min_qt_inter_slice;
    uint8_t                 ph_log2_diff_max_tt_min_qt_inter_slice;
    uint8_t                 ph_cu_qp_delta_subdiv_inter_slice;
    uint8_t                 ph_cu_chroma_qp_offset_subdiv_inter_slice;
    /** \brief Reserved for future use, must be zero */
    uint16_t                reserved16b02;
    uint32_t                reserved32b03[VA_PADDING_LOW - 2];

    union {
        struct {
            uint32_t        ph_non_ref_pic_flag                                             : 1;
            uint32_t        ph_alf_enabled_flag                                             : 1;
            uint32_t        ph_alf_cb_enabled_flag                                          : 1;
            uint32_t        ph_alf_cr_enabled_flag                                          : 1;
            uint32_t        ph_alf_cc_cb_enabled_flag                                       : 1;
            uint32_t        ph_alf_cc_cr_enabled_flag                                       : 1;
            uint32_t        ph_lmcs_enabled_flag                                            : 1;
            uint32_t        ph_chroma_residual_scale_flag                                   : 1;
            uint32_t        ph_explicit_scaling_list_enabled_flag                           : 1;
            uint32_t        ph_virtual_boundaries_present_flag                              : 1;
            uint32_t        ph_temporal_mvp_enabled_flag                                    : 1;
            uint32_t        ph_mmvd_fullpel_only_flag                                       : 1;
            uint32_t        ph_mvd_l1_zero_flag                                             : 1;
            uint32_t        ph_bdof_disabled_flag                                           : 1;
            uint32_t        ph_dmvr_disabled_flag                                           : 1;
            uint32_t        ph_prof_disabled_flag                                           : 1;
            uint32_t        ph_joint_cbcr_sign_flag                                         : 1;
            uint32_t        ph_sao_luma_enabled_flag                                        : 1;
            uint32_t        ph_sao_chroma_enabled_flag                                      : 1;
            uint32_t        ph_deblocking_filter_disabled_flag                              : 1;
            /** \brief Reserved for future use, must be zero */
            uint32_t        reserved                                                        : 12;
        } bits;
        uint32_t            value;
    } ph_flags;

    /** \brief Reserved for future use, must be zero */
    uint32_t                reserved32b04;

    union {
        struct {
            /** \brief Flag to indicate if current picture is an intra picture.
             *  Takes value 1 when all slices of current picture are intra slices.
             *  Takes value 0 when some slices of current picture may not be
             *  intra slices.
             */
            uint32_t        IntraPicFlag                                                    : 1;    // [0..1]
            /** \brief Reserved for future use, must be zero */
            uint32_t        reserved                                                        : 31;
        } fields;
        uint32_t            value;
    } PicMiscFlags;

    /** \brief Reserved bytes for future use, must be zero */
    uint32_t                reserved32b[VA_PADDING_HIGH + 1];

} VAPictureParameterBufferVVC;

/**
 * \brief VVC Slice Parameter Buffer Structure
 *
 * VASliceParameterBufferVVC structure should be accompanied by a
 * slice data buffer, which holds the whole packed slice NAL unit bit stream
 * with emulation prevention bytes not removed.
 *
 * This structure conveys parameters related to slice header and should
 * be sent once per slice.
 */
typedef struct  _VASliceParameterBufferVVC {
    /** @name Codec-independent Slice Parameter Buffer base. */

    /**@{*/

    /** \brief Number of bytes in the slice data buffer for this slice
     * counting from and including NAL unit header.
     */
    uint32_t                slice_data_size;
    /** \brief The offset to the NAL unit header for this slice */
    uint32_t                slice_data_offset;
    /** \brief Slice data buffer flags. See \c VA_SLICE_DATA_FLAG_XXX. */
    uint32_t                slice_data_flag;
    /**
     * \brief Byte offset from NAL unit header to the beginning of slice_data().
     *
     * This byte offset is relative to and includes the NAL unit header
     * and represents the number of bytes parsed in the slice_header()
     * after the removal of any emulation prevention bytes in
     * there. However, the slice data buffer passed to the hardware is
     * the original bitstream, thus including any emulation prevention
     * bytes.
     */
    uint32_t                slice_data_byte_offset;
    /** \brief index into ReferenceFrames[]
     * RefPicList[][] corresponds to VVC spec variable with the same name.
     * Value range [0..14, 0xFF], where 0xFF indicates invalid entry.
     */
    uint8_t                 RefPicList[2][15];

    /**
     * \brief the subpicture ID of the subpicture that contains the slice.
     * The value of the variable CurrSubpicIdx
     * is derived to be such that SubpicIdVal[CurrSubpicIdx] is equal
     * to sh_subpic_id. CurrSubpicIdx is the index of array VASubPicArrayBufferVVC.SubPicSet[].
     * And it is the spec variable with the same name.
     */
    uint16_t                sh_subpic_id;
    /* parameters below are VVC syntax or spec variables. */
    uint16_t                sh_slice_address;
    uint16_t                sh_num_tiles_in_slice_minus1;
    uint8_t                 sh_slice_type;
    uint8_t                 sh_num_alf_aps_ids_luma;
    uint8_t                 sh_alf_aps_id_luma[7];
    uint8_t                 sh_alf_aps_id_chroma;
    uint8_t                 sh_alf_cc_cb_aps_id;
    uint8_t                 sh_alf_cc_cr_aps_id;
    /**
     * \brief NumRefIdxActive[i] - 1 specifies the maximum reference index
     * for RPL i that may be used to decode the slice. When NumRefIdxActive[i]
     * is equal to 0, no reference index for RPL i is used to decode the slice.
     * NumRefIdxActive[] corresponds to VVC spec variable with the same name.
     * It is derived according to formula (138) in VVC spec section 7.4.8.
     */
    uint8_t                 NumRefIdxActive[2];
    uint8_t                 sh_collocated_ref_idx;
    /**
     * \brief initial value of the QpY quantization parameter for the slice.
     * SliceQpY corresponds to VVC spec variable with the same name.
     * It is derived according to formula (86) in VVC spec section 7.4.3.8
     * and formula (139) in VVC Spec section 7.4.8.
     */
    int8_t                  SliceQpY;
    /* parameters below are VVC syntax. */
    int8_t                  sh_cb_qp_offset;
    int8_t                  sh_cr_qp_offset;
    int8_t                  sh_joint_cbcr_qp_offset;
    int8_t                  sh_luma_beta_offset_div2;
    int8_t                  sh_luma_tc_offset_div2;
    int8_t                  sh_cb_beta_offset_div2;
    int8_t                  sh_cb_tc_offset_div2;
    int8_t                  sh_cr_beta_offset_div2;
    int8_t                  sh_cr_tc_offset_div2;
    /** \brief Reserved bytes for future use, must be zero */
    uint8_t                 reserved8b[VA_PADDING_LOW - 1];
    uint32_t                reserved32b;

    // weighted prediction info
    VAWeightedPredInfo      WPInfo;

    union {
        struct {
            /* flags below are VVC syntax. */
            uint32_t        sh_alf_enabled_flag                                             : 1;
            uint32_t        sh_alf_cb_enabled_flag                                          : 1;
            uint32_t        sh_alf_cr_enabled_flag                                          : 1;
            uint32_t        sh_alf_cc_cb_enabled_flag                                       : 1;
            uint32_t        sh_alf_cc_cr_enabled_flag                                       : 1;
            uint32_t        sh_lmcs_used_flag                                               : 1;
            uint32_t        sh_explicit_scaling_list_used_flag                              : 1;
            uint32_t        sh_cabac_init_flag                                              : 1;
            uint32_t        sh_collocated_from_l0_flag                                      : 1;
            uint32_t        sh_cu_chroma_qp_offset_enabled_flag                             : 1;
            uint32_t        sh_sao_luma_used_flag                                           : 1;
            uint32_t        sh_sao_chroma_used_flag                                         : 1;
            uint32_t        sh_deblocking_filter_disabled_flag                              : 1;
            uint32_t        sh_dep_quant_used_flag                                          : 1;
            uint32_t        sh_sign_data_hiding_used_flag                                   : 1;
            uint32_t        sh_ts_residual_coding_disabled_flag                             : 1;
            /** \brief Reserved for future use, must be zero */
            uint32_t        reserved                                                        : 16;
        } bits;
        uint32_t            value;
    } sh_flags;

    /** \brief Reserved bytes for future use, must be zero */
    uint32_t                va_reserved[VA_PADDING_MEDIUM];
} VASliceParameterBufferVVC;

/**
 * \brief VVC Scaling List Data Structure
 *
 * Host decoder sends in an array of VVC Scaling Lists through one or multiple
 * buffers which may contain 1 to 8 VAScalingListVVC data structures in total.
 * Each buffer contains an integer number of VAScalingListVVC data structures
 * with no gap in between.
 * Driver may store the data internally. Host decoder may choose not to
 * send the same scaling list data for each frame. When a VAScalingListVVC
 * structure carries a same value of aps_adaptation_parameter_set_id
 * as a previously stored structure, driver should override the old structure
 * with values in the new structure.
 * VAIQMatrixBufferType is used to send this buffer.
 */
typedef struct _VAScalingListVVC {
    /** \brief VVC syntax to specify the identifier for the APS.*/
    uint8_t                 aps_adaptation_parameter_set_id;
    /** \brief Reserved for future use, must be zero */
    uint8_t                 reserved8b;
    /**
     * \brief Specifies the spec variable ScalingMatrixDCRec[id−14],
     * where id = [14..27].
     */
    uint8_t                 ScalingMatrixDCRec[14];
    /**
     * \brief Specifies the spec variable ScalingMatrixRec[id][x][y],
     * where id = [0..1]. Check section 7.4.3.20 for derivation process.
     */
    uint8_t                 ScalingMatrixRec2x2[2][2][2];
    /**
     * \brief Specifies the spec variable ScalingMatrixRec[id][x][y],
     * where id = [2..7]. Check section 7.4.3.20 for derivation process.
     */
    uint8_t                 ScalingMatrixRec4x4[6][4][4];
    /**
     * \brief Specifies the spec variable ScalingMatrixRec[id][x][y],
     * where id = [8..27]. Check section 7.4.3.20 for derivation process.
     */
    uint8_t                 ScalingMatrixRec8x8[20][8][8];

    /** \brief Reserved bytes for future use, must be zero */
    uint32_t                va_reserved[VA_PADDING_MEDIUM];
} VAScalingListVVC;

/**
 * \brief VVC Adaptive Loop Filter Data Structure
 *
 * Host decoder sends in an array of VVC ALF sets through one or multiple
 * buffers which may contain 1 to 8 VAAlfDataVVC data structures in total.
 * Each buffer contains an integer number of VAAlfDataVVC data structures
 * with no gap in between.
 * Driver may store the data internally. Host decoder may choose not to
 * send the same ALF data for each frame. When a VAAlfDataVVC structure
 * carries a same value of aps_adaptation_parameter_set_id as a previously
 * stored structure, driver should override the old structure
 * with values in the new structure.
 * VAAlfBufferType is used to send this buffer.
 */
typedef struct _VAAlfDataVVC {
    /**
     * \brief VVC Adaptive Loop Filter parameters.
     * All the parameters except reserved bytes are VVC syntax or spec variables.
     */
    uint8_t                 aps_adaptation_parameter_set_id;
    uint8_t                 alf_luma_num_filters_signalled_minus1;
    uint8_t                 alf_luma_coeff_delta_idx[25];
    int8_t                  filtCoeff[25][12];
    uint8_t                 alf_luma_clip_idx[25][12];
    uint8_t                 alf_chroma_num_alt_filters_minus1;
    int8_t                  AlfCoeffC[8][6];
    uint8_t                 alf_chroma_clip_idx[8][6];
    uint8_t                 alf_cc_cb_filters_signalled_minus1;
    int8_t                  CcAlfApsCoeffCb[4][7];
    uint8_t                 alf_cc_cr_filters_signalled_minus1;
    int8_t                  CcAlfApsCoeffCr[4][7];
    /** \brief Reserved bytes for future use, must be zero */
    uint16_t                reserved16b;
    uint32_t                reserved32b;

    union {
        struct {
            uint32_t        alf_luma_filter_signal_flag                                     : 1;
            uint32_t        alf_chroma_filter_signal_flag                                   : 1;
            uint32_t        alf_cc_cb_filter_signal_flag                                    : 1;
            uint32_t        alf_cc_cr_filter_signal_flag                                    : 1;
            uint32_t        alf_luma_clip_flag                                              : 1;
            uint32_t        alf_chroma_clip_flag                                            : 1;
            /** \brief Reserved for future use, must be zero */
            uint32_t        reserved                                                        : 26;
        } bits;
        uint32_t            value;
    } alf_flags;

    /** \brief Reserved for future use, must be zero */
    uint32_t                va_reserved[VA_PADDING_MEDIUM];
} VAAlfDataVVC;

/**
 * \brief VVC Luma Mapping with Chroma Scaling Data Structure
 *
 * Host decoder sends in an array of VVC LMCS sets through one or multiple
 * buffers which may contain 1 to 4 VALmcsDataVVC data structures in total.
 * Each buffer contains an integer number of VALmcsDataVVC data structures
 * with no gap in between.
 * Driver may store the data internally. Host decoder may choose not to
 * send the same LMCS data for each frame. When a VALmcsDataVVC structure
 * carries a same value of aps_adaptation_parameter_set_id as a previously
 * stored structure, driver should override the old structure
 * with values in the new structure.
 * VALmcsBufferType is used to send this buffer.
 */
typedef struct _VALmcsDataVVC {
    /**
     * \brief VVC Luma Mapping with Chroma Scaling parameters.
     * All the parameters except reserved bytes are VVC syntax or spec variables.
     */
    uint8_t                 aps_adaptation_parameter_set_id;
    uint8_t                 lmcs_min_bin_idx;
    uint8_t                 lmcs_delta_max_bin_idx;
    int16_t                 lmcsDeltaCW[16];
    int8_t                  lmcsDeltaCrs;
    /** \brief Reserved for future use, must be zero */
    uint8_t                 reserved8b[VA_PADDING_LOW - 1];
    uint32_t                va_reserved[VA_PADDING_MEDIUM];
} VALmcsDataVVC;

/**
 * \brief VVC SubPicture Data Structure
 *
 * Host decoder sends in an array of VVC SubPic sets through one or
 * multiple buffers which contain sps_num_subpics_minus1 + 1
 * VASubPicVVC data structures in total. Each buffer contains
 * an integer number of VASubPicVVC data structures with no gap in between.
 * The Subpic sets are sent sequentially in the order of indices
 * from 0 to sps_num_subpics_minus1 according to the bitstream.
 * VASubPicBufferType is used to send this buffer.
 */
typedef struct _VASubPicVVC {
    /**
     * \brief VVC SubPicture layout parameters.
     * All the parameters except reserved bytes are VVC syntax or spec variables.
     */
    uint16_t                sps_subpic_ctu_top_left_x;
    uint16_t                sps_subpic_ctu_top_left_y;
    uint16_t                sps_subpic_width_minus1;
    uint16_t                sps_subpic_height_minus1;
    /** \brief the subpicture ID of the i-th subpicture.
     *  It is same variable as in VVC spec.
     */
    uint16_t                SubpicIdVal;

    union {
        struct {
            uint16_t        sps_subpic_treated_as_pic_flag                                  : 1;
            uint16_t        sps_loop_filter_across_subpic_enabled_flag                      : 1;
            /** \brief Reserved for future use, must be zero */
            uint16_t        reserved                                                        : 14;
        } bits;
        uint16_t            value;
    } subpic_flags;

    /** \brief Reserved for future use, must be zero */
    uint32_t                va_reserved[VA_PADDING_LOW];
} VASubPicVVC;

/**
 * \brief data buffer of tile widths and heights.
 * VATileBufferType is used to send this buffer.
 *
 * Host decoder sends in number of pps_num_exp_tile_columns_minus1 + 1
 * tile column widths of pps_tile_column_width_minus1[i], followed by
 * number of pps_num_exp_tile_rows_minus1 + 1 of tile row heights of
 * pps_tile_row_height_minus1[i], through one or multiple buffers.
 * Each tile width or height is formatted as
     uint16_t                tile_dimension;
 * Each buffer contains an integer number of tile_dimension with
 * no gap in between.
 * The buffers with type VATileBufferType should be submitted for each
 * picture. And driver will derive the tile structure from it.
 * When pps_num_exp_tile_columns_minus1 + pps_num_exp_tile_rows_minus1 equals 0,
 * this buffer is still submitted by app to driver.
 */


/**
  * \brief VVC SliceStruct Data Structure
  *
  * Host decoder sends in an array of SliceStruct sets through one or multiple
  * buffers. These SliceStruct sets contain only the "explicit" slices parsed
  * from PPS header.
  * Each SliceStruct set is described by VASliceStructVVC data structure.
  * Each buffer contains an integer number of VASliceStructVVC data structures,
  * which are laid out sequentially in the order of
  * ascending slice indices according to the spec with no gap in between.
  *
  * When pps_rect_slice_flag equals 0 or there are no explicit slices,
  * this buffer is not submitted by app to driver. Otherwise, for each picture,
  * this buffer should be submitted.
  *
  * Note: When pps_slice_width_in_tiles_minus1 + pps_slice_height_in_tiles_minus1
  * equals 0, if the sum of pps_exp_slice_height_in_ctus_minus1 + 1 of all those
  * slices with same SliceTopLeftTileIdx value is less than the height of tile
  * SliceTopLeftTileIdx in unit of CTUs, driver should derive the rest slices in
  * that tile according to equation (21) in spec section 6.5.1. And VASliceStructVVC
  * for these (derived) slices are not passed in to LibVA by App.
  *
  * App should populate the data entries regardless of values of
  * pps_single_slice_per_subpic_flag or sps_subpic_info_present_flag.
  *
  * VASliceStructBufferType is used to send this buffer.
  */
typedef struct _VASliceStructVVC {
    /** \brief the tile index of which the starting CTU (top-left) of
     *  the slice belongs to. The tile index is in raster scan order.
     *  Same syntax variable as in VVC spec.
     */
    uint16_t                SliceTopLeftTileIdx;
    /* plus 1 specifies the width of the rectangular slice in units
     * of tile columns.
     */
    uint16_t                pps_slice_width_in_tiles_minus1;
    /* plus 1 specifies the height of the rectangular slice in units
     * of tile rows. If the slice does not cover the whole tile,
     * pps_slice_height_in_tiles_minus1 shall be 0.
     */
    uint16_t                pps_slice_height_in_tiles_minus1;
    /* plus 1 specifies the height of the rectangular slice in units
     * of CTU rows.
     * If pps_slice_width_in_tiles_minus1 + pps_slice_height_in_tiles_minus1 > 0,
     * set this value to 0.
     * If pps_slice_width_in_tiles_minus1 + pps_slice_height_in_tiles_minus1 == 0,
     * and if there is only one slice in tile, set this value to the number of
     * CTU rows of the tile minus 1, otherwise, set the value equal to
     * corresponding pps_exp_slice_height_in_ctus_minus1 from bitstream.
     */
    uint16_t                pps_exp_slice_height_in_ctus_minus1;

    /** \brief Reserved for future use, must be zero */
    uint32_t                va_reserved[VA_PADDING_LOW];
} VASliceStructVVC;


/**@}*/

#ifdef __cplusplus
}
#endif

#endif /* VA_DEC_VVC_H */
