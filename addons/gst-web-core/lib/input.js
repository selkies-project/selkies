/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/*
 * Licensed to the Apache Software Foundation (ASF) under one
 * or more contributor license agreements.  See the NOTICE file
 * distributed with this work for additional information
 * regarding copyright ownership.  The ASF licenses this file
 * to you under the Apache License, Version 2.0 (the
 * "License"); you may not use this file except in compliance
 * with the License.  You may obtain a copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing,
 * software distributed under the License is distributed on an
 * "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
 * KIND, either express or implied.  See the License for the
 * specific language governing permissions and limitations
 * under the License.
 */

import { GamepadManager } from './gamepad.js';
import { Queue } from './util.js';

/**
 * Class used by frontend to whitelist elements for input
 */
const WHITELIST_CLASS = 'allow-native-input';

const KeyTable = {
    XK_VoidSymbol:                  0xffffff,
    XK_BackSpace:                   0xff08,
    XK_Tab:                         0xff09,
    XK_Linefeed:                    0xff0a,
    XK_Clear:                       0xff0b,
    XK_Return:                      0xff0d,
    XK_Pause:                       0xff13,
    XK_Scroll_Lock:                 0xff14,
    XK_Sys_Req:                     0xff15,
    XK_Escape:                      0xff1b,
    XK_Delete:                      0xffff,
    XK_Multi_key:                   0xff20,
    XK_Codeinput:                   0xff37,
    XK_SingleCandidate:             0xff3c,
    XK_MultipleCandidate:           0xff3d,
    XK_PreviousCandidate:           0xff3e,
    XK_Kanji:                       0xff21,
    XK_Muhenkan:                    0xff22,
    XK_Henkan_Mode:                 0xff23,
    XK_Henkan:                      0xff23,
    XK_Romaji:                      0xff24,
    XK_Hiragana:                    0xff25,
    XK_Katakana:                    0xff26,
    XK_Hiragana_Katakana:           0xff27,
    XK_Zenkaku:                     0xff28,
    XK_Hankaku:                     0xff29,
    XK_Zenkaku_Hankaku:             0xff2a,
    XK_Touroku:                     0xff2b,
    XK_Massyo:                      0xff2c,
    XK_Kana_Lock:                   0xff2d,
    XK_Kana_Shift:                  0xff2e,
    XK_Eisu_Shift:                  0xff2f,
    XK_Eisu_toggle:                 0xff30,
    XK_Kanji_Bangou:                0xff37,
    XK_Zen_Koho:                    0xff3d,
    XK_Mae_Koho:                    0xff3e,
    XK_Home:                        0xff50,
    XK_Left:                        0xff51,
    XK_Up:                          0xff52,
    XK_Right:                       0xff53,
    XK_Down:                        0xff54,
    XK_Prior:                       0xff55,
    XK_Page_Up:                     0xff55,
    XK_Next:                        0xff56,
    XK_Page_Down:                   0xff56,
    XK_End:                         0xff57,
    XK_Begin:                       0xff58,
    XK_Select:                      0xff60,
    XK_Print:                       0xff61,
    XK_Execute:                     0xff62,
    XK_Insert:                      0xff63,
    XK_Undo:                        0xff65,
    XK_Redo:                        0xff66,
    XK_Menu:                        0xff67,
    XK_Find:                        0xff68,
    XK_Cancel:                      0xff69,
    XK_Help:                        0xff6a,
    XK_Break:                       0xff6b,
    XK_Mode_switch:                 0xff7e,
    XK_script_switch:               0xff7e,
    XK_Num_Lock:                    0xff7f,
    XK_KP_Space:                    0xff80,
    XK_KP_Tab:                      0xff89,
    XK_KP_Enter:                    0xff8d,
    XK_KP_F1:                       0xff91,
    XK_KP_F2:                       0xff92,
    XK_KP_F3:                       0xff93,
    XK_KP_F4:                       0xff94,
    XK_KP_Home:                     0xff95,
    XK_KP_Left:                     0xff96,
    XK_KP_Up:                       0xff97,
    XK_KP_Right:                    0xff98,
    XK_KP_Down:                     0xff99,
    XK_KP_Prior:                    0xff9a,
    XK_KP_Page_Up:                  0xff9a,
    XK_KP_Next:                     0xff9b,
    XK_KP_Page_Down:                0xff9b,
    XK_KP_End:                      0xff9c,
    XK_KP_Begin:                    0xff9d,
    XK_KP_Insert:                   0xff9e,
    XK_KP_Delete:                   0xff9f,
    XK_KP_Equal:                    0xffbd,
    XK_KP_Multiply:                 0xffaa,
    XK_KP_Add:                      0xffab,
    XK_KP_Separator:                0xffac,
    XK_KP_Subtract:                 0xffad,
    XK_KP_Decimal:                  0xffae,
    XK_KP_Divide:                   0xffaf,
    XK_KP_0:                        0xffb0,
    XK_KP_1:                        0xffb1,
    XK_KP_2:                        0xffb2,
    XK_KP_3:                        0xffb3,
    XK_KP_4:                        0xffb4,
    XK_KP_5:                        0xffb5,
    XK_KP_6:                        0xffb6,
    XK_KP_7:                        0xffb7,
    XK_KP_8:                        0xffb8,
    XK_KP_9:                        0xffb9,
    XK_F1:                          0xffbe,
    XK_F2:                          0xffbf,
    XK_F3:                          0xffc0,
    XK_F4:                          0xffc1,
    XK_F5:                          0xffc2,
    XK_F6:                          0xffc3,
    XK_F7:                          0xffc4,
    XK_F8:                          0xffc5,
    XK_F9:                          0xffc6,
    XK_F10:                         0xffc7,
    XK_F11:                         0xffc8,
    XK_L1:                          0xffc8,
    XK_F12:                         0xffc9,
    XK_L2:                          0xffc9,
    XK_F13:                         0xffca,
    XK_L3:                          0xffca,
    XK_F14:                         0xffcb,
    XK_L4:                          0xffcb,
    XK_F15:                         0xffcc,
    XK_L5:                          0xffcc,
    XK_F16:                         0xffcd,
    XK_L6:                          0xffcd,
    XK_F17:                         0xffce,
    XK_L7:                          0xffce,
    XK_F18:                         0xffcf,
    XK_L8:                          0xffcf,
    XK_F19:                         0xffd0,
    XK_L9:                          0xffd0,
    XK_F20:                         0xffd1,
    XK_L10:                         0xffd1,
    XK_F21:                         0xffd2,
    XK_R1:                          0xffd2,
    XK_F22:                         0xffd3,
    XK_R2:                          0xffd3,
    XK_F23:                         0xffd4,
    XK_R3:                          0xffd4,
    XK_F24:                         0xffd5,
    XK_R4:                          0xffd5,
    XK_F25:                         0xffd6,
    XK_R5:                          0xffd6,
    XK_F26:                         0xffd7,
    XK_R6:                          0xffd7,
    XK_F27:                         0xffd8,
    XK_R7:                          0xffd8,
    XK_F28:                         0xffd9,
    XK_R8:                          0xffd9,
    XK_F29:                         0xffda,
    XK_R9:                          0xffda,
    XK_F30:                         0xffdb,
    XK_R10:                         0xffdb,
    XK_F31:                         0xffdc,
    XK_R11:                         0xffdc,
    XK_F32:                         0xffdd,
    XK_R12:                         0xffdd,
    XK_F33:                         0xffde,
    XK_R13:                         0xffde,
    XK_F34:                         0xffdf,
    XK_R14:                         0xffdf,
    XK_F35:                         0xffe0,
    XK_R15:                         0xffe0,
    XK_Shift_L:                     0xffe1,
    XK_Shift_R:                     0xffe2,
    XK_Control_L:                   0xffe3,
    XK_Control_R:                   0xffe4,
    XK_Caps_Lock:                   0xffe5,
    XK_Shift_Lock:                  0xffe6,
    XK_Meta_L:                      0xffe7,
    XK_Meta_R:                      0xffe8,
    XK_Alt_L:                       0xffe9,
    XK_Alt_R:                       0xffea,
    XK_Super_L:                     0xffeb,
    XK_Super_R:                     0xffec,
    XK_Hyper_L:                     0xffed,
    XK_Hyper_R:                     0xffee,
    XK_ISO_Level3_Shift:            0xfe03,
    XK_ISO_Next_Group:              0xfe08,
    XK_ISO_Prev_Group:              0xfe0a,
    XK_ISO_First_Group:             0xfe0c,
    XK_ISO_Last_Group:              0xfe0e,
    XK_space:                       0x0020,
    XK_exclam:                      0x0021,
    XK_quotedbl:                    0x0022,
    XK_numbersign:                  0x0023,
    XK_dollar:                      0x0024,
    XK_percent:                     0x0025,
    XK_ampersand:                   0x0026,
    XK_apostrophe:                  0x0027,
    XK_quoteright:                  0x0027,
    XK_parenleft:                   0x0028,
    XK_parenright:                  0x0029,
    XK_asterisk:                    0x002a,
    XK_plus:                        0x002b,
    XK_comma:                       0x002c,
    XK_minus:                       0x002d,
    XK_period:                      0x002e,
    XK_slash:                       0x002f,
    XK_0:                           0x0030,
    XK_1:                           0x0031,
    XK_2:                           0x0032,
    XK_3:                           0x0033,
    XK_4:                           0x0034,
    XK_5:                           0x0035,
    XK_6:                           0x0036,
    XK_7:                           0x0037,
    XK_8:                           0x0038,
    XK_9:                           0x0039,
    XK_colon:                       0x003a,
    XK_semicolon:                   0x003b,
    XK_less:                        0x003c,
    XK_equal:                       0x003d,
    XK_greater:                     0x003e,
    XK_question:                    0x003f,
    XK_at:                          0x0040,
    XK_A:                           0x0041,
    XK_B:                           0x0042,
    XK_C:                           0x0043,
    XK_D:                           0x0044,
    XK_E:                           0x0045,
    XK_F:                           0x0046,
    XK_G:                           0x0047,
    XK_H:                           0x0048,
    XK_I:                           0x0049,
    XK_J:                           0x004a,
    XK_K:                           0x004b,
    XK_L:                           0x004c,
    XK_M:                           0x004d,
    XK_N:                           0x004e,
    XK_O:                           0x004f,
    XK_P:                           0x0050,
    XK_Q:                           0x0051,
    XK_R:                           0x0052,
    XK_S:                           0x0053,
    XK_T:                           0x0054,
    XK_U:                           0x0055,
    XK_V:                           0x0056,
    XK_W:                           0x0057,
    XK_X:                           0x0058,
    XK_Y:                           0x0059,
    XK_Z:                           0x005a,
    XK_bracketleft:                 0x005b,
    XK_backslash:                   0x005c,
    XK_bracketright:                0x005d,
    XK_asciicircum:                 0x005e,
    XK_underscore:                  0x005f,
    XK_grave:                       0x0060,
    XK_quoteleft:                   0x0060,
    XK_a:                           0x0061,
    XK_b:                           0x0062,
    XK_c:                           0x0063,
    XK_d:                           0x0064,
    XK_e:                           0x0065,
    XK_f:                           0x0066,
    XK_g:                           0x0067,
    XK_h:                           0x0068,
    XK_i:                           0x0069,
    XK_j:                           0x006a,
    XK_k:                           0x006b,
    XK_l:                           0x006c,
    XK_m:                           0x006d,
    XK_n:                           0x006e,
    XK_o:                           0x006f,
    XK_p:                           0x0070,
    XK_q:                           0x0071,
    XK_r:                           0x0072,
    XK_s:                           0x0073,
    XK_t:                           0x0074,
    XK_u:                           0x0075,
    XK_v:                           0x0076,
    XK_w:                           0x0077,
    XK_x:                           0x0078,
    XK_y:                           0x0079,
    XK_z:                           0x007a,
    XK_braceleft:                   0x007b,
    XK_bar:                         0x007c,
    XK_braceright:                  0x007d,
    XK_asciitilde:                  0x007e,
    XK_nobreakspace:                0x00a0,
    XK_exclamdown:                  0x00a1,
    XK_cent:                        0x00a2,
    XK_sterling:                    0x00a3,
    XK_currency:                    0x00a4,
    XK_yen:                         0x00a5,
    XK_brokenbar:                   0x00a6,
    XK_section:                     0x00a7,
    XK_diaeresis:                   0x00a8,
    XK_copyright:                   0x00a9,
    XK_ordfeminine:                 0x00aa,
    XK_guillemotleft:               0x00ab,
    XK_notsign:                     0x00ac,
    XK_hyphen:                      0x00ad,
    XK_registered:                  0x00ae,
    XK_macron:                      0x00af,
    XK_degree:                      0x00b0,
    XK_plusminus:                   0x00b1,
    XK_twosuperior:                 0x00b2,
    XK_threesuperior:               0x00b3,
    XK_acute:                       0x00b4,
    XK_mu:                          0x00b5,
    XK_paragraph:                   0x00b6,
    XK_periodcentered:              0x00b7,
    XK_cedilla:                     0x00b8,
    XK_onesuperior:                 0x00b9,
    XK_masculine:                   0x00ba,
    XK_guillemotright:              0x00bb,
    XK_onequarter:                  0x00bc,
    XK_onehalf:                     0x00bd,
    XK_threequarters:               0x00be,
    XK_questiondown:                0x00bf,
    XK_Agrave:                      0x00c0,
    XK_Aacute:                      0x00c1,
    XK_Acircumflex:                 0x00c2,
    XK_Atilde:                      0x00c3,
    XK_Adiaeresis:                  0x00c4,
    XK_Aring:                       0x00c5,
    XK_AE:                          0x00c6,
    XK_Ccedilla:                    0x00c7,
    XK_Egrave:                      0x00c8,
    XK_Eacute:                      0x00c9,
    XK_Ecircumflex:                 0x00ca,
    XK_Ediaeresis:                  0x00cb,
    XK_Igrave:                      0x00cc,
    XK_Iacute:                      0x00cd,
    XK_Icircumflex:                 0x00ce,
    XK_Idiaeresis:                  0x00cf,
    XK_ETH:                         0x00d0,
    XK_Eth:                         0x00d0,
    XK_Ntilde:                      0x00d1,
    XK_Ograve:                      0x00d2,
    XK_Oacute:                      0x00d3,
    XK_Ocircumflex:                 0x00d4,
    XK_Otilde:                      0x00d5,
    XK_Odiaeresis:                  0x00d6,
    XK_multiply:                    0x00d7,
    XK_Oslash:                      0x00d8,
    XK_Ooblique:                    0x00d8,
    XK_Ugrave:                      0x00d9,
    XK_Uacute:                      0x00da,
    XK_Ucircumflex:                 0x00db,
    XK_Udiaeresis:                  0x00dc,
    XK_Yacute:                      0x00dd,
    XK_THORN:                       0x00de,
    XK_Thorn:                       0x00de,
    XK_ssharp:                      0x00df,
    XK_agrave:                      0x00e0,
    XK_aacute:                      0x00e1,
    XK_acircumflex:                 0x00e2,
    XK_atilde:                      0x00e3,
    XK_adiaeresis:                  0x00e4,
    XK_aring:                       0x00e5,
    XK_ae:                          0x00e6,
    XK_ccedilla:                    0x00e7,
    XK_egrave:                      0x00e8,
    XK_eacute:                      0x00e9,
    XK_ecircumflex:                 0x00ea,
    XK_ediaeresis:                  0x00eb,
    XK_igrave:                      0x00ec,
    XK_iacute:                      0x00ed,
    XK_icircumflex:                 0x00ee,
    XK_idiaeresis:                  0x00ef,
    XK_eth:                         0x00f0,
    XK_ntilde:                      0x00f1,
    XK_ograve:                      0x00f2,
    XK_oacute:                      0x00f3,
    XK_ocircumflex:                 0x00f4,
    XK_otilde:                      0x00f5,
    XK_odiaeresis:                  0x00f6,
    XK_division:                    0x00f7,
    XK_oslash:                      0x00f8,
    XK_ooblique:                    0x00f8,
    XK_ugrave:                      0x00f9,
    XK_uacute:                      0x00fa,
    XK_ucircumflex:                 0x00fb,
    XK_udiaeresis:                  0x00fc,
    XK_yacute:                      0x00fd,
    XK_thorn:                       0x00fe,
    XK_ydiaeresis:                  0x00ff,
    XK_Hangul:                      0xff31,
    XK_Hangul_Hanja:                0xff34,
    XK_Hangul_Jeonja:               0xff38,
    XF86XK_ModeLock:                0x1008FF01,
    XF86XK_MonBrightnessUp:         0x1008FF02,
    XF86XK_MonBrightnessDown:       0x1008FF03,
    XF86XK_KbdLightOnOff:           0x1008FF04,
    XF86XK_KbdBrightnessUp:         0x1008FF05,
    XF86XK_KbdBrightnessDown:       0x1008FF06,
    XF86XK_Standby:                 0x1008FF10,
    XF86XK_AudioLowerVolume:        0x1008FF11,
    XF86XK_AudioMute:               0x1008FF12,
    XF86XK_AudioRaiseVolume:        0x1008FF13,
    XF86XK_AudioPlay:               0x1008FF14,
    XF86XK_AudioStop:               0x1008FF15,
    XF86XK_AudioPrev:               0x1008FF16,
    XF86XK_AudioNext:               0x1008FF17,
    XF86XK_HomePage:                0x1008FF18,
    XF86XK_Mail:                    0x1008FF19,
    XF86XK_Start:                   0x1008FF1A,
    XF86XK_Search:                  0x1008FF1B,
    XF86XK_AudioRecord:             0x1008FF1C,
    XF86XK_Calculator:              0x1008FF1D,
    XF86XK_Memo:                    0x1008FF1E,
    XF86XK_ToDoList:                0x1008FF1F,
    XF86XK_Calendar:                0x1008FF20,
    XF86XK_PowerDown:               0x1008FF21,
    XF86XK_ContrastAdjust:          0x1008FF22,
    XF86XK_RockerUp:                0x1008FF23,
    XF86XK_RockerDown:              0x1008FF24,
    XF86XK_RockerEnter:             0x1008FF25,
    XF86XK_Back:                    0x1008FF26,
    XF86XK_Forward:                 0x1008FF27,
    XF86XK_Stop:                    0x1008FF28,
    XF86XK_Refresh:                 0x1008FF29,
    XF86XK_PowerOff:                0x1008FF2A,
    XF86XK_WakeUp:                  0x1008FF2B,
    XF86XK_Eject:                   0x1008FF2C,
    XF86XK_ScreenSaver:             0x1008FF2D,
    XF86XK_WWW:                     0x1008FF2E,
    XF86XK_Sleep:                   0x1008FF2F,
    XF86XK_Favorites:               0x1008FF30,
    XF86XK_AudioPause:              0x1008FF31,
    XF86XK_AudioMedia:              0x1008FF32,
    XF86XK_MyComputer:              0x1008FF33,
    XF86XK_VendorHome:              0x1008FF34,
    XF86XK_LightBulb:               0x1008FF35,
    XF86XK_Shop:                    0x1008FF36,
    XF86XK_History:                 0x1008FF37,
    XF86XK_OpenURL:                 0x1008FF38,
    XF86XK_AddFavorite:             0x1008FF39,
    XF86XK_HotLinks:                0x1008FF3A,
    XF86XK_BrightnessAdjust:        0x1008FF3B,
    XF86XK_Finance:                 0x1008FF3C,
    XF86XK_Community:               0x1008FF3D,
    XF86XK_AudioRewind:             0x1008FF3E,
    XF86XK_BackForward:             0x1008FF3F,
    XF86XK_Launch0:                 0x1008FF40,
    XF86XK_Launch1:                 0x1008FF41,
    XF86XK_Launch2:                 0x1008FF42,
    XF86XK_Launch3:                 0x1008FF43,
    XF86XK_Launch4:                 0x1008FF44,
    XF86XK_Launch5:                 0x1008FF45,
    XF86XK_Launch6:                 0x1008FF46,
    XF86XK_Launch7:                 0x1008FF47,
    XF86XK_Launch8:                 0x1008FF48,
    XF86XK_Launch9:                 0x1008FF49,
    XF86XK_LaunchA:                 0x1008FF4A,
    XF86XK_LaunchB:                 0x1008FF4B,
    XF86XK_LaunchC:                 0x1008FF4C,
    XF86XK_LaunchD:                 0x1008FF4D,
    XF86XK_LaunchE:                 0x1008FF4E,
    XF86XK_LaunchF:                 0x1008FF4F,
    XF86XK_ApplicationLeft:         0x1008FF50,
    XF86XK_ApplicationRight:        0x1008FF51,
    XF86XK_Book:                    0x1008FF52,
    XF86XK_CD:                      0x1008FF53,
    XF86XK_Calculater:              0x1008FF54,
    XF86XK_Clear:                   0x1008FF55,
    XF86XK_Close:                   0x1008FF56,
    XF86XK_Copy:                    0x1008FF57,
    XF86XK_Cut:                     0x1008FF58,
    XF86XK_Display:                 0x1008FF59,
    XF86XK_DOS:                     0x1008FF5A,
    XF86XK_Documents:               0x1008FF5B,
    XF86XK_Excel:                   0x1008FF5C,
    XF86XK_Explorer:                0x1008FF5D,
    XF86XK_Game:                    0x1008FF5E,
    XF86XK_Go:                      0x1008FF5F,
    XF86XK_iTouch:                  0x1008FF60,
    XF86XK_LogOff:                  0x1008FF61,
    XF86XK_Market:                  0x1008FF62,
    XF86XK_Meeting:                 0x1008FF63,
    XF86XK_MenuKB:                  0x1008FF65,
    XF86XK_MenuPB:                  0x1008FF66,
    XF86XK_MySites:                 0x1008FF67,
    XF86XK_New:                     0x1008FF68,
    XF86XK_News:                    0x1008FF69,
    XF86XK_OfficeHome:              0x1008FF6A,
    XF86XK_Open:                    0x1008FF6B,
    XF86XK_Option:                  0x1008FF6C,
    XF86XK_Paste:                   0x1008FF6D,
    XF86XK_Phone:                   0x1008FF6E,
    XF86XK_Q:                       0x1008FF70,
    XF86XK_Reply:                   0x1008FF72,
    XF86XK_Reload:                  0x1008FF73,
    XF86XK_RotateWindows:           0x1008FF74,
    XF86XK_RotationPB:              0x1008FF75,
    XF86XK_RotationKB:              0x1008FF76,
    XF86XK_Save:                    0x1008FF77,
    XF86XK_ScrollUp:                0x1008FF78,
    XF86XK_ScrollDown:              0x1008FF79,
    XF86XK_ScrollClick:             0x1008FF7A,
    XF86XK_Send:                    0x1008FF7B,
    XF86XK_Spell:                   0x1008FF7C,
    XF86XK_SplitScreen:             0x1008FF7D,
    XF86XK_Support:                 0x1008FF7E,
    XF86XK_TaskPane:                0x1008FF7F,
    XF86XK_Terminal:                0x1008FF80,
    XF86XK_Tools:                   0x1008FF81,
    XF86XK_Travel:                  0x1008FF82,
    XF86XK_UserPB:                  0x1008FF84,
    XF86XK_User1KB:                 0x1008FF85,
    XF86XK_User2KB:                 0x1008FF86,
    XF86XK_Video:                   0x1008FF87,
    XF86XK_WheelButton:             0x1008FF88,
    XF86XK_Word:                    0x1008FF89,
    XF86XK_Xfer:                    0x1008FF8A,
    XF86XK_ZoomIn:                  0x1008FF8B,
    XF86XK_ZoomOut:                 0x1008FF8C,
    XF86XK_Away:                    0x1008FF8D,
    XF86XK_Messenger:               0x1008FF8E,
    XF86XK_WebCam:                  0x1008FF8F,
    XF86XK_MailForward:             0x1008FF90,
    XF86XK_Pictures:                0x1008FF91,
    XF86XK_Music:                   0x1008FF92,
    XF86XK_Battery:                 0x1008FF93,
    XF86XK_Bluetooth:               0x1008FF94,
    XF86XK_WLAN:                    0x1008FF95,
    XF86XK_UWB:                     0x1008FF96,
    XF86XK_AudioForward:            0x1008FF97,
    XF86XK_AudioRepeat:             0x1008FF98,
    XF86XK_AudioRandomPlay:         0x1008FF99,
    XF86XK_Subtitle:                0x1008FF9A,
    XF86XK_AudioCycleTrack:         0x1008FF9B,
    XF86XK_CycleAngle:              0x1008FF9C,
    XF86XK_FrameBack:               0x1008FF9D,
    XF86XK_FrameForward:            0x1008FF9E,
    XF86XK_Time:                    0x1008FF9F,
    XF86XK_Select:                  0x1008FFA0,
    XF86XK_View:                    0x1008FFA1,
    XF86XK_TopMenu:                 0x1008FFA2,
    XF86XK_Red:                     0x1008FFA3,
    XF86XK_Green:                   0x1008FFA4,
    XF86XK_Yellow:                  0x1008FFA5,
    XF86XK_Blue:                    0x1008FFA6,
    XF86XK_Suspend:                 0x1008FFA7,
    XF86XK_Hibernate:               0x1008FFA8,
    XF86XK_TouchpadToggle:          0x1008FFA9,
    XF86XK_TouchpadOn:              0x1008FFB0,
    XF86XK_TouchpadOff:             0x1008FFB1,
    XF86XK_AudioMicMute:            0x1008FFB2,
    XF86XK_Switch_VT_1:             0x1008FE01,
    XF86XK_Switch_VT_2:             0x1008FE02,
    XF86XK_Switch_VT_3:             0x1008FE03,
    XF86XK_Switch_VT_4:             0x1008FE04,
    XF86XK_Switch_VT_5:             0x1008FE05,
    XF86XK_Switch_VT_6:             0x1008FE06,
    XF86XK_Switch_VT_7:             0x1008FE07,
    XF86XK_Switch_VT_8:             0x1008FE08,
    XF86XK_Switch_VT_9:             0x1008FE09,
    XF86XK_Switch_VT_10:            0x1008FE0A,
    XF86XK_Switch_VT_11:            0x1008FE0B,
    XF86XK_Switch_VT_12:            0x1008FE0C,
    XF86XK_Ungrab:                  0x1008FE20,
    XF86XK_ClearGrab:               0x1008FE21,
    XF86XK_Next_VMode:              0x1008FE22,
    XF86XK_Prev_VMode:              0x1008FE23,
    XF86XK_LogWindowTree:           0x1008FE24,
    XF86XK_LogGrabInfo:             0x1008FE25,
};

const keysymsByCodepoint = {
    0x0100: 0x03c0, 0x0101: 0x03e0, 0x0102: 0x01c3, 0x0103: 0x01e3, 0x0104: 0x01a1, 0x0105: 0x01b1,
    0x0106: 0x01c6, 0x0107: 0x01e6, 0x0108: 0x02c6, 0x0109: 0x02e6, 0x010a: 0x02c5, 0x010b: 0x02e5,
    0x010c: 0x01c8, 0x010d: 0x01e8, 0x010e: 0x01cf, 0x010f: 0x01ef, 0x0110: 0x01d0, 0x0111: 0x01f0,
    0x0112: 0x03aa, 0x0113: 0x03ba, 0x0116: 0x03cc, 0x0117: 0x03ec, 0x0118: 0x01ca, 0x0119: 0x01ea,
    0x011a: 0x01cc, 0x011b: 0x01ec, 0x011c: 0x02d8, 0x011d: 0x02f8, 0x011e: 0x02ab, 0x011f: 0x02bb,
    0x0120: 0x02d5, 0x0121: 0x02f5, 0x0122: 0x03ab, 0x0123: 0x03bb, 0x0124: 0x02a6, 0x0125: 0x02b6,
    0x0126: 0x02a1, 0x0127: 0x02b1, 0x0128: 0x03a5, 0x0129: 0x03b5, 0x012a: 0x03cf, 0x012b: 0x03ef,
    0x012e: 0x03c7, 0x012f: 0x03e7, 0x0130: 0x02a9, 0x0131: 0x02b9, 0x0134: 0x02ac, 0x0135: 0x02bc,
    0x0136: 0x03d3, 0x0137: 0x03f3, 0x0138: 0x03a2, 0x0139: 0x01c5, 0x013a: 0x01e5, 0x013b: 0x03a6,
    0x013c: 0x03b6, 0x013d: 0x01a5, 0x013e: 0x01b5, 0x0141: 0x01a3, 0x0142: 0x01b3, 0x0143: 0x01d1,
    0x0144: 0x01f1, 0x0145: 0x03d1, 0x0146: 0x03f1, 0x0147: 0x01d2, 0x0148: 0x01f2, 0x014a: 0x03bd,
    0x014b: 0x03bf, 0x014c: 0x03d2, 0x014d: 0x03f2, 0x0150: 0x01d5, 0x0151: 0x01f5, 0x0152: 0x13bc,
    0x0153: 0x13bd, 0x0154: 0x01c0, 0x0155: 0x01e0, 0x0156: 0x03a3, 0x0157: 0x03b3, 0x0158: 0x01d8,
    0x0159: 0x01f8, 0x015a: 0x01a6, 0x015b: 0x01b6, 0x015c: 0x02de, 0x015d: 0x02fe, 0x015e: 0x01aa,
    0x015f: 0x01ba, 0x0160: 0x01a9, 0x0161: 0x01b9, 0x0162: 0x01de, 0x0163: 0x01fe, 0x0164: 0x01ab,
    0x0165: 0x01bb, 0x0166: 0x03ac, 0x0167: 0x03bc, 0x0168: 0x03dd, 0x0169: 0x03fd, 0x016a: 0x03de,
    0x016b: 0x03fe, 0x016c: 0x02dd, 0x016d: 0x02fd, 0x016e: 0x01d9, 0x016f: 0x01f9, 0x0170: 0x01db,
    0x0171: 0x01fb, 0x0172: 0x03d9, 0x0173: 0x03f9, 0x0178: 0x13be, 0x0179: 0x01ac, 0x017a: 0x01bc,
    0x017b: 0x01af, 0x017c: 0x01bf, 0x017d: 0x01ae, 0x017e: 0x01be, 0x0192: 0x08f6, 0x01d2: 0x10001d1,
    0x02c7: 0x01b7, 0x02d8: 0x01a2, 0x02d9: 0x01ff, 0x02db: 0x01b2, 0x02dd: 0x01bd, 0x0385: 0x07ae,
    0x0386: 0x07a1, 0x0388: 0x07a2, 0x0389: 0x07a3, 0x038a: 0x07a4, 0x038c: 0x07a7, 0x038e: 0x07a8,
    0x038f: 0x07ab, 0x0390: 0x07b6, 0x0391: 0x07c1, 0x0392: 0x07c2, 0x0393: 0x07c3, 0x0394: 0x07c4,
    0x0395: 0x07c5, 0x0396: 0x07c6, 0x0397: 0x07c7, 0x0398: 0x07c8, 0x0399: 0x07c9, 0x039a: 0x07ca,
    0x039b: 0x07cb, 0x039c: 0x07cc, 0x039d: 0x07cd, 0x039e: 0x07ce, 0x039f: 0x07cf, 0x03a0: 0x07d0,
    0x03a1: 0x07d1, 0x03a3: 0x07d2, 0x03a4: 0x07d4, 0x03a5: 0x07d5, 0x03a6: 0x07d6, 0x03a7: 0x07d7,
    0x03a8: 0x07d8, 0x03a9: 0x07d9, 0x03aa: 0x07a5, 0x03ab: 0x07a9, 0x03ac: 0x07b1, 0x03ad: 0x07b2,
    0x03ae: 0x07b3, 0x03af: 0x07b4, 0x03b0: 0x07ba, 0x03b1: 0x07e1, 0x03b2: 0x07e2, 0x03b3: 0x07e3,
    0x03b4: 0x07e4, 0x03b5: 0x07e5, 0x03b6: 0x07e6, 0x03b7: 0x07e7, 0x03b8: 0x07e8, 0x03b9: 0x07e9,
    0x03ba: 0x07ea, 0x03bb: 0x07eb, 0x03bc: 0x07ec, 0x03bd: 0x07ed, 0x03be: 0x07ee, 0x03bf: 0x07ef,
    0x03c0: 0x07f0, 0x03c1: 0x07f1, 0x03c2: 0x07f3, 0x03c3: 0x07f2, 0x03c4: 0x07f4, 0x03c5: 0x07f5,
    0x03c6: 0x07f6, 0x03c7: 0x07f7, 0x03c8: 0x07f8, 0x03c9: 0x07f9, 0x03ca: 0x07b5, 0x03cb: 0x07b9,
    0x03cc: 0x07b7, 0x03cd: 0x07b8, 0x03ce: 0x07bb, 0x0401: 0x06b3, 0x0402: 0x06b1, 0x0403: 0x06b2,
    0x0404: 0x06b4, 0x0405: 0x06b5, 0x0406: 0x06b6, 0x0407: 0x06b7, 0x0408: 0x06b8, 0x0409: 0x06b9,
    0x040a: 0x06ba, 0x040b: 0x06bb, 0x040c: 0x06bc, 0x040e: 0x06be, 0x040f: 0x06bf, 0x0410: 0x06e1,
    0x0411: 0x06e2, 0x0412: 0x06f7, 0x0413: 0x06e7, 0x0414: 0x06e4, 0x0415: 0x06e5, 0x0416: 0x06f6,
    0x0417: 0x06fa, 0x0418: 0x06e9, 0x0419: 0x06ea, 0x041a: 0x06eb, 0x041b: 0x06ec, 0x041c: 0x06ed,
    0x041d: 0x06ee, 0x041e: 0x06ef, 0x041f: 0x06f0, 0x0420: 0x06f2, 0x0421: 0x06f3, 0x0422: 0x06f4,
    0x0423: 0x06f5, 0x0424: 0x06e6, 0x0425: 0x06e8, 0x0426: 0x06e3, 0x0427: 0x06fe, 0x0428: 0x06fb,
    0x0429: 0x06fd, 0x042a: 0x06ff, 0x042b: 0x06f9, 0x042c: 0x06f8, 0x042d: 0x06fc, 0x042e: 0x06e0,
    0x042f: 0x06f1, 0x0430: 0x06c1, 0x0431: 0x06c2, 0x0432: 0x06d7, 0x0433: 0x06c7, 0x0434: 0x06c4,
    0x0435: 0x06c5, 0x0436: 0x06d6, 0x0437: 0x06da, 0x0438: 0x06c9, 0x0439: 0x06ca, 0x043a: 0x06cb,
    0x043b: 0x06cc, 0x043c: 0x06cd, 0x043d: 0x06ce, 0x043e: 0x06cf, 0x043f: 0x06d0, 0x0440: 0x06d2,
    0x0441: 0x06d3, 0x0442: 0x06d4, 0x0443: 0x06d5, 0x0444: 0x06c6, 0x0445: 0x06c8, 0x0446: 0x06c3,
    0x0447: 0x06de, 0x0448: 0x06db, 0x0449: 0x06dd, 0x044a: 0x06df, 0x044b: 0x06d9, 0x044c: 0x06d8,
    0x044d: 0x06dc, 0x044e: 0x06c0, 0x044f: 0x06d1, 0x0451: 0x06a3, 0x0452: 0x06a1, 0x0453: 0x06a2,
    0x0454: 0x06a4, 0x0455: 0x06a5, 0x0456: 0x06a6, 0x0457: 0x06a7, 0x0458: 0x06a8, 0x0459: 0x06a9,
    0x045a: 0x06aa, 0x045b: 0x06ab, 0x045c: 0x06ac, 0x045e: 0x06ae, 0x045f: 0x06af, 0x0490: 0x06bd,
    0x0491: 0x06ad, 0x05d0: 0x0ce0, 0x05d1: 0x0ce1, 0x05d2: 0x0ce2, 0x05d3: 0x0ce3, 0x05d4: 0x0ce4,
    0x05d5: 0x0ce5, 0x05d6: 0x0ce6, 0x05d7: 0x0ce7, 0x05d8: 0x0ce8, 0x05d9: 0x0ce9, 0x05da: 0x0cea,
    0x05db: 0x0ceb, 0x05dc: 0x0cec, 0x05dd: 0x0ced, 0x05de: 0x0cee, 0x05df: 0x0cef, 0x05e0: 0x0cf0,
    0x05e1: 0x0cf1, 0x05e2: 0x0cf2, 0x05e3: 0x0cf3, 0x05e4: 0x0cf4, 0x05e5: 0x0cf5, 0x05e6: 0x0cf6,
    0x05e7: 0x0cf7, 0x05e8: 0x0cf8, 0x05e9: 0x0cf9, 0x05ea: 0x0cfa, 0x060c: 0x05ac, 0x061b: 0x05bb,
    0x061f: 0x05bf, 0x0621: 0x05c1, 0x0622: 0x05c2, 0x0623: 0x05c3, 0x0624: 0x05c4, 0x0625: 0x05c5,
    0x0626: 0x05c6, 0x0627: 0x05c7, 0x0628: 0x05c8, 0x0629: 0x05c9, 0x062a: 0x05ca, 0x062b: 0x05cb,
    0x062c: 0x05cc, 0x062d: 0x05cd, 0x062e: 0x05ce, 0x062f: 0x05cf, 0x0630: 0x05d0, 0x0631: 0x05d1,
    0x0632: 0x05d2, 0x0633: 0x05d3, 0x0634: 0x05d4, 0x0635: 0x05d5, 0x0636: 0x05d6, 0x0637: 0x05d7,
    0x0638: 0x05d8, 0x0639: 0x05d9, 0x063a: 0x05da, 0x0640: 0x05e0, 0x0641: 0x05e1, 0x0642: 0x05e2,
    0x0643: 0x05e3, 0x0644: 0x05e4, 0x0645: 0x05e5, 0x0646: 0x05e6, 0x0647: 0x05e7, 0x0648: 0x05e8,
    0x0649: 0x05e9, 0x064a: 0x05ea, 0x064b: 0x05eb, 0x064c: 0x05ec, 0x064d: 0x05ed, 0x064e: 0x05ee,
    0x064f: 0x05ef, 0x0650: 0x05f0, 0x0651: 0x05f1, 0x0652: 0x05f2, 0x0e01: 0x0da1, 0x0e02: 0x0da2,
    0x0e03: 0x0da3, 0x0e04: 0x0da4, 0x0e05: 0x0da5, 0x0e06: 0x0da6, 0x0e07: 0x0da7, 0x0e08: 0x0da8,
    0x0e09: 0x0da9, 0x0e0a: 0x0daa, 0x0e0b: 0x0dab, 0x0e0c: 0x0dac, 0x0e0d: 0x0dad, 0x0e0e: 0x0dae,
    0x0e0f: 0x0daf, 0x0e10: 0x0db0, 0x0e11: 0x0db1, 0x0e12: 0x0db2, 0x0e13: 0x0db3, 0x0e14: 0x0db4,
    0x0e15: 0x0db5, 0x0e16: 0x0db6, 0x0e17: 0x0db7, 0x0e18: 0x0db8, 0x0e19: 0x0db9, 0x0e1a: 0x0dba,
    0x0e1b: 0x0dbb, 0x0e1c: 0x0dbc, 0x0e1d: 0x0dbd, 0x0e1e: 0x0dbe, 0x0e1f: 0x0dbf, 0x0e20: 0x0dc0,
    0x0e21: 0x0dc1, 0x0e22: 0x0dc2, 0x0e23: 0x0dc3, 0x0e24: 0x0dc4, 0x0e25: 0x0dc5, 0x0e26: 0x0dc6,
    0x0e27: 0x0dc7, 0x0e28: 0x0dc8, 0x0e29: 0x0dc9, 0x0e2a: 0x0dca, 0x0e2b: 0x0dcb, 0x0e2c: 0x0dcc,
    0x0e2d: 0x0dcd, 0x0e2e: 0x0dce, 0x0e2f: 0x0dcf, 0x0e30: 0x0dd0, 0x0e31: 0x0dd1, 0x0e32: 0x0dd2,
    0x0e33: 0x0dd3, 0x0e34: 0x0dd4, 0x0e35: 0x0dd5, 0x0e36: 0x0dd6, 0x0e37: 0x0dd7, 0x0e38: 0x0dd8,
    0x0e39: 0x0dd9, 0x0e3a: 0x0dda, 0x0e3f: 0x0ddf, 0x0e40: 0x0de0, 0x0e41: 0x0de1, 0x0e42: 0x0de2,
    0x0e43: 0x0de3, 0x0e44: 0x0de4, 0x0e45: 0x0de5, 0x0e46: 0x0de6, 0x0e47: 0x0de7, 0x0e48: 0x0de8,
    0x0e49: 0x0de9, 0x0e4a: 0x0dea, 0x0e4b: 0x0deb, 0x0e4c: 0x0dec, 0x0e4d: 0x0ded, 0x0e50: 0x0df0,
    0x0e51: 0x0df1, 0x0e52: 0x0df2, 0x0e53: 0x0df3, 0x0e54: 0x0df4, 0x0e55: 0x0df5, 0x0e56: 0x0df6,
    0x0e57: 0x0df7, 0x0e58: 0x0df8, 0x0e59: 0x0df9, 0x2002: 0x0aa2, 0x2003: 0x0aa1, 0x2004: 0x0aa3,
    0x2005: 0x0aa4, 0x2007: 0x0aa5, 0x2008: 0x0aa6, 0x2009: 0x0aa7, 0x200a: 0x0aa8, 0x2012: 0x0abb,
    0x2013: 0x0aaa, 0x2014: 0x0aa9, 0x2015: 0x07af, 0x2017: 0x0cdf, 0x2018: 0x0ad0, 0x2019: 0x0ad1,
    0x201a: 0x0afd, 0x201c: 0x0ad2, 0x201d: 0x0ad3, 0x201e: 0x0afe, 0x2020: 0x0af1, 0x2021: 0x0af2,
    0x2022: 0x0ae6, 0x2025: 0x0aaf, 0x2026: 0x0aae, 0x2030: 0x0ad5, 0x2032: 0x0ad6, 0x2033: 0x0ad7,
    0x2038: 0x0afc, 0x203e: 0x047e, 0x20a9: 0x0eff, 0x20ac: 0x20ac, 0x2105: 0x0ab8, 0x2116: 0x06b0,
    0x2117: 0x0afb, 0x211e: 0x0ad4, 0x2122: 0x0ac9, 0x2153: 0x0ab0, 0x2154: 0x0ab1, 0x2155: 0x0ab2,
    0x2156: 0x0ab3, 0x2157: 0x0ab4, 0x2158: 0x0ab5, 0x2159: 0x0ab6, 0x215a: 0x0ab7, 0x215b: 0x0ac3,
    0x215c: 0x0ac4, 0x215d: 0x0ac5, 0x215e: 0x0ac6, 0x2190: 0x08fb, 0x2191: 0x08fc, 0x2192: 0x08fd,
    0x2193: 0x08fe, 0x21d2: 0x08ce, 0x21d4: 0x08cd, 0x2202: 0x08ef, 0x2207: 0x08c5, 0x2218: 0x0bca,
    0x221a: 0x08d6, 0x221d: 0x08c1, 0x221e: 0x08c2, 0x2227: 0x08de, 0x2228: 0x08df, 0x2229: 0x08dc,
    0x222a: 0x08dd, 0x222b: 0x08bf, 0x2234: 0x08c0, 0x223c: 0x08c8, 0x2243: 0x08c9, 0x2245: 0x1002248,
    0x2260: 0x08bd, 0x2261: 0x08cf, 0x2264: 0x08bc, 0x2265: 0x08be, 0x2282: 0x08da, 0x2283: 0x08db,
    0x22a2: 0x0bfc, 0x22a3: 0x0bdc, 0x22a4: 0x0bc2, 0x22a5: 0x0bce, 0x2308: 0x0bd3, 0x230a: 0x0bc4,
    0x2315: 0x0afa, 0x2320: 0x08a4, 0x2321: 0x08a5, 0x2395: 0x0bcc, 0x239b: 0x08ab, 0x239d: 0x08ac,
    0x239e: 0x08ad, 0x23a0: 0x08ae, 0x23a1: 0x08a7, 0x23a3: 0x08a8, 0x23a4: 0x08a9, 0x23a6: 0x08aa,
    0x23a8: 0x08af, 0x23ac: 0x08b0, 0x23b7: 0x08a1, 0x23ba: 0x09ef, 0x23bb: 0x09f0, 0x23bc: 0x09f2,
    0x23bd: 0x09f3, 0x2409: 0x09e2, 0x240a: 0x09e5, 0x240b: 0x09e9, 0x240c: 0x09e3, 0x240d: 0x09e4,
    0x2423: 0x0aac, 0x2424: 0x09e8, 0x2500: 0x08a3, 0x2502: 0x08a6, 0x250c: 0x08a2, 0x2510: 0x09eb,
    0x2514: 0x09ed, 0x2518: 0x09ea, 0x251c: 0x09f4, 0x2524: 0x09f5, 0x252c: 0x09f7, 0x2534: 0x09f6,
    0x253c: 0x09ee, 0x2592: 0x09e1, 0x25aa: 0x0ae7, 0x25ab: 0x0ae1, 0x25ac: 0x0adb, 0x25ad: 0x0ae2,
    0x25ae: 0x0adf, 0x25af: 0x0acf, 0x25b2: 0x0ae8, 0x25b3: 0x0ae3, 0x25b6: 0x0add, 0x25b7: 0x0acd,
    0x25bc: 0x0ae9, 0x25bd: 0x0ae4, 0x25c0: 0x0adc, 0x25c1: 0x0acc, 0x25c6: 0x09e0, 0x25cb: 0x0ace,
    0x25cf: 0x0ade, 0x25e6: 0x0ae0, 0x2606: 0x0ae5, 0x260e: 0x0af9, 0x2613: 0x0aca, 0x261c: 0x0aea,
    0x261e: 0x0aeb, 0x2640: 0x0af8, 0x2642: 0x0af7, 0x2663: 0x0aec, 0x2665: 0x0aee, 0x2666: 0x0aed,
    0x266d: 0x0af6, 0x266f: 0x0af5, 0x2713: 0x0af3, 0x2717: 0x0af4, 0x271d: 0x0ad9, 0x2720: 0x0af0,
    0x27e8: 0x0abc, 0x27e9: 0x0abe, 0x3001: 0x04a4, 0x3002: 0x04a1, 0x300c: 0x04a2, 0x300d: 0x04a3,
    0x309b: 0x04de, 0x309c: 0x04df, 0x30a1: 0x04a7, 0x30a2: 0x04b1, 0x30a3: 0x04a8, 0x30a4: 0x04b2,
    0x30a5: 0x04a9, 0x30a6: 0x04b3, 0x30a7: 0x04aa, 0x30a8: 0x04b4, 0x030a9: 0x04ab, 0x30aa: 0x04b5,
    0x30ab: 0x04b6, 0x30ad: 0x04b7, 0x30af: 0x04b8, 0x30b1: 0x04b9, 0x30b3: 0x04ba, 0x30b5: 0x04bb,
    0x30b7: 0x04bc, 0x30b9: 0x04bd, 0x30bb: 0x04be, 0x30bd: 0x04bf, 0x30bf: 0x04c0, 0x30c1: 0x04c1,
    0x30c3: 0x04af, 0x30c4: 0x04c2, 0x30c6: 0x04c3, 0x30c8: 0x04c4, 0x30ca: 0x04c5, 0x30cb: 0x04c6,
    0x30cc: 0x04c7, 0x30cd: 0x04c8, 0x30ce: 0x04c9, 0x30cf: 0x04ca, 0x30d2: 0x04cb, 0x30d5: 0x04cc,
    0x30d8: 0x04cd, 0x30db: 0x04ce, 0x30de: 0x04cf, 0x30df: 0x04d0, 0x30e0: 0x04d1, 0x30e1: 0x04d2,
    0x30e2: 0x04d3, 0x30e3: 0x04ac, 0x30e4: 0x04d4, 0x30e5: 0x04ad, 0x30e6: 0x04d5, 0x30e7: 0x04ae,
    0x30e8: 0x04d6, 0x30e9: 0x04d7, 0x30ea: 0x04d8, 0x30eb: 0x04d9, 0x30ec: 0x04da, 0x30ed: 0x04db,
    0x30ef: 0x04dc, 0x30f2: 0x04a6, 0x30f3: 0x04dd, 0x30fb: 0x04a5, 0x30fc: 0x04b0,
};
const Keysyms = {
    lookup: function(u) {
        if ((u >= 0x20) && (u <= 0xff)) { return u; }
        const keysym = keysymsByCodepoint[u];
        if (keysym !== undefined) { return keysym; }
        return 0x01000000 | u;
    }
};

const DOMKeyTable = {};
(function() {
    function addStandard(key, standard) {
        if (standard === undefined) throw new Error("Undefined keysym for key \"" + key + "\"");
        if (key in DOMKeyTable) throw new Error("Duplicate entry for key \"" + key + "\"");
        DOMKeyTable[key] = [standard, standard, standard, standard];
    }
    function addLeftRight(key, left, right) {
        if (left === undefined) throw new Error("Undefined keysym for key \"" + key + "\"");
        if (right === undefined) throw new Error("Undefined keysym for key \"" + key + "\"");
        if (key in DOMKeyTable) throw new Error("Duplicate entry for key \"" + key + "\"");
        DOMKeyTable[key] = [left, left, right, left];
    }
    function addNumpad(key, standard, numpad) {
        if (standard === undefined) throw new Error("Undefined keysym for key \"" + key + "\"");
        if (numpad === undefined) throw new Error("Undefined keysym for key \"" + key + "\"");
        if (key in DOMKeyTable) throw new Error("Duplicate entry for key \"" + key + "\"");
        DOMKeyTable[key] = [standard, standard, standard, numpad];
    }
    addLeftRight("Alt", KeyTable.XK_Alt_L, KeyTable.XK_Alt_R);
    addStandard("AltGraph", KeyTable.XK_ISO_Level3_Shift);
    addStandard("CapsLock", KeyTable.XK_Caps_Lock);
    addLeftRight("Control", KeyTable.XK_Control_L, KeyTable.XK_Control_R);
    addLeftRight("Meta", KeyTable.XK_Super_L, KeyTable.XK_Super_R);
    addStandard("NumLock", KeyTable.XK_Num_Lock);
    addStandard("ScrollLock", KeyTable.XK_Scroll_Lock);
    addLeftRight("Shift", KeyTable.XK_Shift_L, KeyTable.XK_Shift_R);
    addNumpad("Enter", KeyTable.XK_Return, KeyTable.XK_KP_Enter);
    addStandard("Tab", KeyTable.XK_Tab);
    addNumpad(" ", KeyTable.XK_space, KeyTable.XK_KP_Space);
    addNumpad("ArrowDown", KeyTable.XK_Down, KeyTable.XK_KP_Down);
    addNumpad("ArrowLeft", KeyTable.XK_Left, KeyTable.XK_KP_Left);
    addNumpad("ArrowRight", KeyTable.XK_Right, KeyTable.XK_KP_Right);
    addNumpad("ArrowUp", KeyTable.XK_Up, KeyTable.XK_KP_Up);
    addNumpad("End", KeyTable.XK_End, KeyTable.XK_KP_End);
    addNumpad("Home", KeyTable.XK_Home, KeyTable.XK_KP_Home);
    addNumpad("PageDown", KeyTable.XK_Next, KeyTable.XK_KP_Next);
    addNumpad("PageUp", KeyTable.XK_Prior, KeyTable.XK_KP_Prior);
    addStandard("Backspace", KeyTable.XK_BackSpace);
    addNumpad("Clear", KeyTable.XK_Clear, KeyTable.XK_KP_Begin);
    addStandard("Copy", KeyTable.XF86XK_Copy);
    addStandard("Cut", KeyTable.XF86XK_Cut);
    addNumpad("Delete", KeyTable.XK_Delete, KeyTable.XK_KP_Delete);
    addNumpad("Insert", KeyTable.XK_Insert, KeyTable.XK_KP_Insert);
    addStandard("Paste", KeyTable.XF86XK_Paste);
    addStandard("Redo", KeyTable.XK_Redo);
    addStandard("Undo", KeyTable.XK_Undo);
    addStandard("Cancel", KeyTable.XK_Cancel);
    addStandard("ContextMenu", KeyTable.XK_Menu);
    addStandard("Escape", KeyTable.XK_Escape);
    addStandard("Execute", KeyTable.XK_Execute);
    addStandard("Find", KeyTable.XK_Find);
    addStandard("Help", KeyTable.XK_Help);
    addStandard("Pause", KeyTable.XK_Pause);
    addStandard("Select", KeyTable.XK_Select);
    addStandard("ZoomIn", KeyTable.XF86XK_ZoomIn);
    addStandard("ZoomOut", KeyTable.XF86XK_ZoomOut);
    addStandard("BrightnessDown", KeyTable.XF86XK_MonBrightnessDown);
    addStandard("BrightnessUp", KeyTable.XF86XK_MonBrightnessUp);
    addStandard("Eject", KeyTable.XF86XK_Eject);
    addStandard("LogOff", KeyTable.XF86XK_LogOff);
    addStandard("Power", KeyTable.XF86XK_PowerOff);
    addStandard("PowerOff", KeyTable.XF86XK_PowerDown);
    addStandard("PrintScreen", KeyTable.XK_Print);
    addStandard("Hibernate", KeyTable.XF86XK_Hibernate);
    addStandard("Standby", KeyTable.XF86XK_Standby);
    addStandard("WakeUp", KeyTable.XF86XK_WakeUp);
    addStandard("AllCandidates", KeyTable.XK_MultipleCandidate);
    addStandard("Alphanumeric", KeyTable.XK_Eisu_toggle);
    addStandard("CodeInput", KeyTable.XK_Codeinput);
    addStandard("Compose", KeyTable.XK_Multi_key);
    addStandard("Convert", KeyTable.XK_Henkan);
    addStandard("GroupFirst", KeyTable.XK_ISO_First_Group);
    addStandard("GroupLast", KeyTable.XK_ISO_Last_Group);
    addStandard("GroupNext", KeyTable.XK_ISO_Next_Group);
    addStandard("GroupPrevious", KeyTable.XK_ISO_Prev_Group);
    addStandard("NonConvert", KeyTable.XK_Muhenkan);
    addStandard("PreviousCandidate", KeyTable.XK_PreviousCandidate);
    addStandard("SingleCandidate", KeyTable.XK_SingleCandidate);
    addStandard("HangulMode", KeyTable.XK_Hangul);
    addStandard("HanjaMode", KeyTable.XK_Hangul_Hanja);
    addStandard("JunjaMode", KeyTable.XK_Hangul_Jeonja);
    addStandard("Eisu", KeyTable.XK_Eisu_toggle);
    addStandard("Hankaku", KeyTable.XK_Hankaku);
    addStandard("Hiragana", KeyTable.XK_Hiragana);
    addStandard("HiraganaKatakana", KeyTable.XK_Hiragana_Katakana);
    addStandard("KanaMode", KeyTable.XK_Kana_Shift);
    addStandard("KanjiMode", KeyTable.XK_Kanji);
    addStandard("Katakana", KeyTable.XK_Katakana);
    addStandard("Romaji", KeyTable.XK_Romaji);
    addStandard("Zenkaku", KeyTable.XK_Zenkaku);
    addStandard("ZenkakuHankaku", KeyTable.XK_Zenkaku_Hankaku);
    addStandard("F1", KeyTable.XK_F1); addStandard("F2", KeyTable.XK_F2); addStandard("F3", KeyTable.XK_F3);
    addStandard("F4", KeyTable.XK_F4); addStandard("F5", KeyTable.XK_F5); addStandard("F6", KeyTable.XK_F6);
    addStandard("F7", KeyTable.XK_F7); addStandard("F8", KeyTable.XK_F8); addStandard("F9", KeyTable.XK_F9);
    addStandard("F10", KeyTable.XK_F10); addStandard("F11", KeyTable.XK_F11); addStandard("F12", KeyTable.XK_F12);
    addStandard("F13", KeyTable.XK_F13); addStandard("F14", KeyTable.XK_F14); addStandard("F15", KeyTable.XK_F15);
    addStandard("F16", KeyTable.XK_F16); addStandard("F17", KeyTable.XK_F17); addStandard("F18", KeyTable.XK_F18);
    addStandard("F19", KeyTable.XK_F19); addStandard("F20", KeyTable.XK_F20); addStandard("F21", KeyTable.XK_F21);
    addStandard("F22", KeyTable.XK_F22); addStandard("F23", KeyTable.XK_F23); addStandard("F24", KeyTable.XK_F24);
    addStandard("F25", KeyTable.XK_F25); addStandard("F26", KeyTable.XK_F26); addStandard("F27", KeyTable.XK_F27);
    addStandard("F28", KeyTable.XK_F28); addStandard("F29", KeyTable.XK_F29); addStandard("F30", KeyTable.XK_F30);
    addStandard("F31", KeyTable.XK_F31); addStandard("F32", KeyTable.XK_F32); addStandard("F33", KeyTable.XK_F33);
    addStandard("F34", KeyTable.XK_F34); addStandard("F35", KeyTable.XK_F35);
    addStandard("Close", KeyTable.XF86XK_Close);
    addStandard("MailForward", KeyTable.XF86XK_MailForward);
    addStandard("MailReply", KeyTable.XF86XK_Reply);
    addStandard("MailSend", KeyTable.XF86XK_Send);
    addStandard("MediaFastForward", KeyTable.XF86XK_AudioForward);
    addStandard("MediaPause", KeyTable.XF86XK_AudioPause);
    addStandard("MediaPlay", KeyTable.XF86XK_AudioPlay);
    addStandard("MediaRecord", KeyTable.XF86XK_AudioRecord);
    addStandard("MediaRewind", KeyTable.XF86XK_AudioRewind);
    addStandard("MediaStop", KeyTable.XF86XK_AudioStop);
    addStandard("MediaTrackNext", KeyTable.XF86XK_AudioNext);
    addStandard("MediaTrackPrevious", KeyTable.XF86XK_AudioPrev);
    addStandard("New", KeyTable.XF86XK_New);
    addStandard("Open", KeyTable.XF86XK_Open);
    addStandard("Print", KeyTable.XK_Print);
    addStandard("Save", KeyTable.XF86XK_Save);
    addStandard("SpellCheck", KeyTable.XF86XK_Spell);
    addStandard("AudioVolumeDown", KeyTable.XF86XK_AudioLowerVolume);
    addStandard("AudioVolumeUp", KeyTable.XF86XK_AudioRaiseVolume);
    addStandard("AudioVolumeMute", KeyTable.XF86XK_AudioMute);
    addStandard("MicrophoneVolumeMute", KeyTable.XF86XK_AudioMicMute);
    addStandard("LaunchApplication1", KeyTable.XF86XK_MyComputer);
    addStandard("LaunchApplication2", KeyTable.XF86XK_Calculator);
    addStandard("LaunchCalendar", KeyTable.XF86XK_Calendar);
    addStandard("LaunchMail", KeyTable.XF86XK_Mail);
    addStandard("LaunchMediaPlayer", KeyTable.XF86XK_AudioMedia);
    addStandard("LaunchMusicPlayer", KeyTable.XF86XK_Music);
    addStandard("LaunchPhone", KeyTable.XF86XK_Phone);
    addStandard("LaunchScreenSaver", KeyTable.XF86XK_ScreenSaver);
    addStandard("LaunchSpreadsheet", KeyTable.XF86XK_Excel);
    addStandard("LaunchWebBrowser", KeyTable.XF86XK_WWW);
    addStandard("LaunchWebCam", KeyTable.XF86XK_WebCam);
    addStandard("LaunchWordProcessor", KeyTable.XF86XK_Word);
    addStandard("BrowserBack", KeyTable.XF86XK_Back);
    addStandard("BrowserFavorites", KeyTable.XF86XK_Favorites);
    addStandard("BrowserForward", KeyTable.XF86XK_Forward);
    addStandard("BrowserHome", KeyTable.XF86XK_HomePage);
    addStandard("BrowserRefresh", KeyTable.XF86XK_Refresh);
    addStandard("BrowserSearch", KeyTable.XF86XK_Search);
    addStandard("BrowserStop", KeyTable.XF86XK_Stop);
    addStandard("Dimmer", KeyTable.XF86XK_BrightnessAdjust);
    addStandard("MediaAudioTrack", KeyTable.XF86XK_AudioCycleTrack);
    addStandard("RandomToggle", KeyTable.XF86XK_AudioRandomPlay);
    addStandard("SplitScreenToggle", KeyTable.XF86XK_SplitScreen);
    addStandard("Subtitle", KeyTable.XF86XK_Subtitle);
    addStandard("VideoModeNext", KeyTable.XF86XK_Next_VMode);
    addNumpad("=", KeyTable.XK_equal, KeyTable.XK_KP_Equal);
    addNumpad("+", KeyTable.XK_plus, KeyTable.XK_KP_Add);
    addNumpad("-", KeyTable.XK_minus, KeyTable.XK_KP_Subtract);
    addNumpad("*", KeyTable.XK_asterisk, KeyTable.XK_KP_Multiply);
    addNumpad("/", KeyTable.XK_slash, KeyTable.XK_KP_Divide);
    addNumpad(".", KeyTable.XK_period, KeyTable.XK_KP_Decimal);
    addNumpad(",", KeyTable.XK_comma, KeyTable.XK_KP_Separator);
    addNumpad("0", KeyTable.XK_0, KeyTable.XK_KP_0);
    addNumpad("1", KeyTable.XK_1, KeyTable.XK_KP_1);
    addNumpad("2", KeyTable.XK_2, KeyTable.XK_KP_2);
    addNumpad("3", KeyTable.XK_3, KeyTable.XK_KP_3);
    addNumpad("4", KeyTable.XK_4, KeyTable.XK_KP_4);
    addNumpad("5", KeyTable.XK_5, KeyTable.XK_KP_5);
    addNumpad("6", KeyTable.XK_6, KeyTable.XK_KP_6);
    addNumpad("7", KeyTable.XK_7, KeyTable.XK_KP_7);
    addNumpad("8", KeyTable.XK_8, KeyTable.XK_KP_8);
    addNumpad("9", KeyTable.XK_9, KeyTable.XK_KP_9);
})();

const vkeys = {
    0x08: 'Backspace', 0x09: 'Tab', 0x0a: 'NumpadClear', 0x0d: 'Enter',
    0x10: 'ShiftLeft', 0x11: 'ControlLeft', 0x12: 'AltLeft', 0x13: 'Pause',
    0x14: 'CapsLock', 0x15: 'Lang1', 0x19: 'Lang2', 0x1b: 'Escape',
    0x1c: 'Convert', 0x1d: 'NonConvert', 0x20: 'Space', 0x21: 'PageUp',
    0x22: 'PageDown', 0x23: 'End', 0x24: 'Home', 0x25: 'ArrowLeft',
    0x26: 'ArrowUp', 0x27: 'ArrowRight', 0x28: 'ArrowDown', 0x29: 'Select',
    0x2c: 'PrintScreen', 0x2d: 'Insert', 0x2e: 'Delete', 0x2f: 'Help',
    0x30: 'Digit0', 0x31: 'Digit1', 0x32: 'Digit2', 0x33: 'Digit3',
    0x34: 'Digit4', 0x35: 'Digit5', 0x36: 'Digit6', 0x37: 'Digit7',
    0x38: 'Digit8', 0x39: 'Digit9', 0x5b: 'MetaLeft', 0x5c: 'MetaRight',
    0x5d: 'ContextMenu', 0x5f: 'Sleep', 0x60: 'Numpad0', 0x61: 'Numpad1',
    0x62: 'Numpad2', 0x63: 'Numpad3', 0x64: 'Numpad4', 0x65: 'Numpad5',
    0x66: 'Numpad6', 0x67: 'Numpad7', 0x68: 'Numpad8', 0x69: 'Numpad9',
    0x6a: 'NumpadMultiply', 0x6b: 'NumpadAdd', 0x6c: 'NumpadDecimal',
    0x6d: 'NumpadSubtract', 0x6e: 'NumpadDecimal', 0x6f: 'NumpadDivide',
    0x70: 'F1', 0x71: 'F2', 0x72: 'F3', 0x73: 'F4', 0x74: 'F5', 0x75: 'F6',
    0x76: 'F7', 0x77: 'F8', 0x78: 'F9', 0x79: 'F10', 0x7a: 'F11', 0x7b: 'F12',
    0x7c: 'F13', 0x7d: 'F14', 0x7e: 'F15', 0x7f: 'F16', 0x80: 'F17', 0x81: 'F18',
    0x82: 'F19', 0x83: 'F20', 0x84: 'F21', 0x85: 'F22', 0x86: 'F23', 0x87: 'F24',
    0x90: 'NumLock', 0x91: 'ScrollLock', 0xa6: 'BrowserBack', 0xa7: 'BrowserForward',
    0xa8: 'BrowserRefresh', 0xa9: 'BrowserStop', 0xaa: 'BrowserSearch',
    0xab: 'BrowserFavorites', 0xac: 'BrowserHome', 0xad: 'AudioVolumeMute',
    0xae: 'AudioVolumeDown', 0xaf: 'AudioVolumeUp', 0xb0: 'MediaTrackNext',
    0xb1: 'MediaTrackPrevious', 0xb2: 'MediaStop', 0xb3: 'MediaPlayPause',
    0xb4: 'LaunchMail', 0xb5: 'MediaSelect', 0xb6: 'LaunchApp1',
    0xb7: 'LaunchApp2', 0xe1: 'AltRight',
};

const fixedkeys = {
    'Backspace': 'Backspace', 'AltLeft': 'Alt', 'AltRight': 'Alt',
    'CapsLock': 'CapsLock', 'ContextMenu': 'ContextMenu', 'ControlLeft': 'Control',
    'ControlRight': 'Control', 'Enter': 'Enter', 'MetaLeft': 'Meta',
    'MetaRight': 'Meta', 'ShiftLeft': 'Shift', 'ShiftRight': 'Shift',
    'Tab': 'Tab', 'Delete': 'Delete', 'End': 'End', 'Help': 'Help',
    'Home': 'Home', 'Insert': 'Insert', 'PageDown': 'PageDown', 'PageUp': 'PageUp',
    'ArrowDown': 'ArrowDown', 'ArrowLeft': 'ArrowLeft', 'ArrowRight': 'ArrowRight',
    'ArrowUp': 'ArrowUp', 'NumLock': 'NumLock', 'NumpadBackspace': 'Backspace',
    'NumpadClear': 'Clear', 'Escape': 'Escape',
    'F1': 'F1', 'F2': 'F2', 'F3': 'F3', 'F4': 'F4', 'F5': 'F5', 'F6': 'F6',
    'F7': 'F7', 'F8': 'F8', 'F9': 'F9', 'F10': 'F10', 'F11': 'F11', 'F12': 'F12',
    'F13': 'F13', 'F14': 'F14', 'F15': 'F15', 'F16': 'F16', 'F17': 'F17', 'F18': 'F18',
    'F19': 'F19', 'F20': 'F20', 'F21': 'F21', 'F22': 'F22', 'F23': 'F23', 'F24': 'F24',
    'F25': 'F25', 'F26': 'F26', 'F27': 'F27', 'F28': 'F28', 'F29': 'F29', 'F30': 'F30',
    'F31': 'F31', 'F32': 'F32', 'F33': 'F33', 'F34': 'F34', 'F35': 'F35',
    'PrintScreen': 'PrintScreen', 'ScrollLock': 'ScrollLock', 'Pause': 'Pause',
    'BrowserBack': 'BrowserBack', 'BrowserFavorites': 'BrowserFavorites',
    'BrowserForward': 'BrowserForward', 'BrowserHome': 'BrowserHome',
    'BrowserRefresh': 'BrowserRefresh', 'BrowserSearch': 'BrowserSearch',
    'BrowserStop': 'BrowserStop', 'Eject': 'Eject', 'LaunchApp1': 'LaunchMyComputer',
    'LaunchApp2': 'LaunchCalendar', 'LaunchMail': 'LaunchMail',
    'MediaPlayPause': 'MediaPlay', 'MediaStop': 'MediaStop',
    'MediaTrackNext': 'MediaTrackNext', 'MediaTrackPrevious': 'MediaTrackPrevious',
    'Power': 'Power', 'Sleep': 'Sleep', 'AudioVolumeDown': 'AudioVolumeDown',
    'AudioVolumeMute': 'AudioVolumeMute', 'AudioVolumeUp': 'AudioVolumeUp',
    'WakeUp': 'WakeUp',
};

const browser = {
    isMac: function() { return /Mac|iPod|iPhone|iPad/.test(navigator.platform); },
    isIOS: function() { return /iPod|iPhone|iPad/.test(navigator.platform); },
    isWindows: function() { return /Win/.test(navigator.platform); },
    isLinux: function() { return /Linux/.test(navigator.platform); },
    isChrome: function() { return !!window.chrome && (!!window.chrome.webstore || !!window.chrome.runtime); },
    isSafari: function() { return /Safari/.test(navigator.userAgent) && !/Chrome/.test(navigator.userAgent); },
};

const NumpadTranslations_NumLockOn = {
    [KeyTable.XK_KP_Space]: KeyTable.XK_space,
    [KeyTable.XK_KP_Enter]: KeyTable.XK_Return,
    [KeyTable.XK_KP_Equal]: KeyTable.XK_equal,
    [KeyTable.XK_KP_Multiply]: KeyTable.XK_asterisk,
    [KeyTable.XK_KP_Add]: KeyTable.XK_plus,
    [KeyTable.XK_KP_Separator]: KeyTable.XK_comma,
    [KeyTable.XK_KP_Subtract]: KeyTable.XK_minus,
    [KeyTable.XK_KP_Decimal]: KeyTable.XK_period,
    [KeyTable.XK_KP_Divide]: KeyTable.XK_slash,
    [KeyTable.XK_KP_0]: KeyTable.XK_0,
    [KeyTable.XK_KP_1]: KeyTable.XK_1,
    [KeyTable.XK_KP_2]: KeyTable.XK_2,
    [KeyTable.XK_KP_3]: KeyTable.XK_3,
    [KeyTable.XK_KP_4]: KeyTable.XK_4,
    [KeyTable.XK_KP_5]: KeyTable.XK_5,
    [KeyTable.XK_KP_6]: KeyTable.XK_6,
    [KeyTable.XK_KP_7]: KeyTable.XK_7,
    [KeyTable.XK_KP_8]: KeyTable.XK_8,
    [KeyTable.XK_KP_9]: KeyTable.XK_9,
};

const NumpadTranslations_NumLockOff = {
    [KeyTable.XK_KP_Home]: KeyTable.XK_Home,
    [KeyTable.XK_KP_Up]: KeyTable.XK_Up,
    [KeyTable.XK_KP_Page_Up]: KeyTable.XK_Page_Up,
    [KeyTable.XK_KP_Prior]: KeyTable.XK_Prior,
    [KeyTable.XK_KP_Left]: KeyTable.XK_Left,
    [KeyTable.XK_KP_Begin]: KeyTable.XK_Clear,
    [KeyTable.XK_KP_Right]: KeyTable.XK_Right,
    [KeyTable.XK_KP_End]: KeyTable.XK_End,
    [KeyTable.XK_KP_Down]: KeyTable.XK_Down,
    [KeyTable.XK_KP_Page_Down]: KeyTable.XK_Page_Down,
    [KeyTable.XK_KP_Next]: KeyTable.XK_Next,
    [KeyTable.XK_KP_Insert]: KeyTable.XK_Insert,
    [KeyTable.XK_KP_Delete]: KeyTable.XK_Delete,
    [KeyTable.XK_KP_Enter]: KeyTable.XK_Return,
};

const KeyboardUtil = {
    getKeyCode: function(evt) {
        if (evt.code) {
            switch (evt.code) {
                case 'OSLeft': return 'MetaLeft';
                case 'OSRight': return 'MetaRight';
            }
            return evt.code;
        }
        if (evt.keyCode in vkeys) {
            let code = vkeys[evt.keyCode];
            if (browser.isMac() && (code === 'ContextMenu')) {
                code = 'MetaRight';
            }
            if (evt.location === 2) {
                switch (code) {
                    case 'ShiftLeft': return 'ShiftRight';
                    case 'ControlLeft': return 'ControlRight';
                    case 'AltLeft': return 'AltRight';
                }
            }
            if (evt.location === 3) {
                switch (code) {
                    case 'Delete': return 'NumpadDecimal';
                    case 'Insert': return 'Numpad0';
                    case 'End': return 'Numpad1';
                    case 'ArrowDown': return 'Numpad2';
                    case 'PageDown': return 'Numpad3';
                    case 'ArrowLeft': return 'Numpad4';
                    case 'ArrowRight': return 'Numpad6';
                    case 'Home': return 'Numpad7';
                    case 'ArrowUp': return 'Numpad8';
                    case 'PageUp': return 'Numpad9';
                    case 'Enter': return 'NumpadEnter';
                }
            }
            return code;
        }
        return 'Unidentified';
    },

    getKey: function(evt) {
        if ((evt.key !== undefined) && (evt.key !== 'Unidentified')  && (evt.key !== 'Dead')) {
            switch (evt.key) {
                case 'OS': return 'Meta';
                case 'LaunchMyComputer': return 'LaunchApplication1';
                case 'LaunchCalculator': return 'LaunchApplication2';
                case 'UIKeyInputUpArrow': return 'ArrowUp';
                case 'UIKeyInputDownArrow': return 'ArrowDown';
                case 'UIKeyInputLeftArrow': return 'ArrowLeft';
                case 'UIKeyInputRightArrow': return 'ArrowRight';
                case 'UIKeyInputEscape': return 'Escape';
            }
            if ((evt.key === '\x00') && (KeyboardUtil.getKeyCode(evt) === 'NumpadDecimal')) {
                return 'Delete';
            }
            return evt.key;
        }
        const code = KeyboardUtil.getKeyCode(evt);
        if (code in fixedkeys) {
            return fixedkeys[code];
        }
        if (evt.charCode) {
            return String.fromCharCode(evt.charCode);
        }
        return 'Unidentified';
    },

    getKeysym: function(evt) {
        const key = KeyboardUtil.getKey(evt);
        if (key === 'Unidentified') {
            return null;
        }

        if (key in DOMKeyTable) {
            let location = evt.location;
            if ((browser.isSafari() && key === 'Meta' && location === 0) || // Safari 12.0.3 (Mojave) MetaRight has location 0
                (browser.isChrome() && key === 'Meta' && location === 0 && KeyboardUtil.getKeyCode(evt) === 'MetaRight')) { // Chrome (Linux) MetaRight has location 0
                location = 2; // DOM_KEY_LOCATION_RIGHT
            }

            if ((key === 'Clear') && (location === 3)) { // Numpad
                let code = KeyboardUtil.getKeyCode(evt);
                if (code === 'NumLock') { // Clear key when numlock is on.
                    location = 0; // DOM_KEY_LOCATION_STANDARD
                }
            }
            if ((location === undefined) || (location > 3)) {
                location = 0;
            }
            if (key === 'Meta') {
                let code = KeyboardUtil.getKeyCode(evt);
                if (code === 'AltLeft') { return KeyTable.XK_Meta_L; }
                if (code === 'AltRight') { return KeyTable.XK_Meta_R; }
            }
            if (key === 'Clear') {
                let code = KeyboardUtil.getKeyCode(evt);
                if (code === 'NumLock') { return KeyTable.XK_Num_Lock; }
            }
            if (browser.isWindows()) {
                switch (key) {
                    case 'Zenkaku': case 'Hankaku': return KeyTable.XK_Zenkaku_Hankaku;
                    case 'Romaji': case 'KanaMode': return KeyTable.XK_Romaji;
                }
            }
            return DOMKeyTable[key][location];
        }

        if (key.length !== 1) {
            return null;
        }
        const codepoint = key.charCodeAt();
        if (codepoint) {
            return Keysyms.lookup(codepoint);
        }
        return null;
    }
};

const _stopEvent = function (e) {
    e.stopPropagation();
    e.preventDefault();
};


export class Input {
    constructor(element, send, isSharedMode = false, playerIndex = 0,  useCssScaling = false, initialSlot = null) {
        this.element = element;
        this.send = send;
        this._isSidebarOpen = false;
        this.isSharedMode = isSharedMode;
        this.controllerSlot = initialSlot;
        this.playerIndex = playerIndex;
        this.cursorDiv = document.createElement('canvas');
        this.cursorDiv.style.position = 'fixed';
        this.cursorDiv.style.pointerEvents = 'none';
        this.cursorDiv.style.zIndex = '999999';
        this.cursorDiv.style.display = 'none';
        this.cursorDiv.style.left = '0px';
        this.cursorDiv.style.top = '0px';
        this.cursorImg = this.cursorDiv.getContext('2d');
        document.body.appendChild(this.cursorDiv);
        this.cursorHotspot = { x: 0, y: 0 };
        this._cursorImageBitmap = null;
        this._rawHotspotX = 0;
        this._rawHotspotY = 0;
        this.use_browser_cursors = false;
        this._latestMouseX = 0;
        this._latestMouseY = 0;
        this.useCssScaling = useCssScaling;
        this.mouseRelative = false;
        this.m = null;
        this.buttonMask = 0;
        this.gamepadManager = null;
        this.x = 0;
        this.y = 0;
        this.onmenuhotkey = null;
        this.onfullscreenhotkey = this.enterFullscreen;
        this.ongamepadconnected = null;
        this.ongamepaddisconneceted = null;
        this.listeners = [];
        this.listeners_context = [];
        this._queue = new Queue();
        this._allowTrackpadScrolling = true;
        this._allowThreshold = true;
        this._smallestDeltaY = 10000;
        this._wheelThreshold = 100;
        this._scrollMagnitude = 10;
        this.cursorScaleFactor = null;
        this._cursorBase64Data = null;

        this._guacKeyboardID = Input._nextGuacID++;
        this._EVENT_MARKER = '_GUAC_KEYBOARD_HANDLED_BY_' + this._guacKeyboardID;

        this._keyDownList = {}; // Maps event.code -> keysym
        this._altGrArmed = false;
        this._altGrTimeout = null;
        this._altGrCtrlTime = 0;
        this._macCmdSwapped = false;

        this._isSynth = false;
        this.isComposing = false;
        this.compositionString = "";
        this.keyboardInputAssist = document.getElementById('keyboard-input-assist');

        this._activeTouches = new Map();
        this._activeTouchIdentifier = null;
        this._isTwoFingerGesture = false;
        this._MIN_SWIPE_DISTANCE = 30;
        this._MAX_SWIPE_DURATION = 600;
        this._VERTICAL_SWIPE_RATIO = 1.5;
        this._SCROLL_PIXELS_PER_TICK = 40;
        this._MAX_SCROLL_MAGNITUDE = 8;
        this._TAP_THRESHOLD_DISTANCE_SQ = 10*10;
        this._TAP_MAX_DURATION = 250;
        this._trackpadMode = false;
        this._trackpadTouches = new Map();
        this._trackpadLastTapTime = 0;
        this._trackpadIsDragging = false;
        this._trackpadTapTimeout = null;
        this._trackpadLastScrollCentroid = null;
        this._touchScrollLastCentroid = null;
        this.inputAttached = false;
    }

    setSharedMode(enabled) {
        this.isSharedMode = !!enabled;
    }

    updateControllerSlot(newSlot) {
        if (this.controllerSlot !== newSlot) {
            console.log(`Input class: Controller slot updated to: ${newSlot}`);
            this.controllerSlot = newSlot;
        }
    }
    _handleVisibilityMessage(event) {
        if (event.origin !== window.location.origin) return;
        const message = event.data;
        if (typeof message === "object" && message !== null && message.type === 'sidebarVisibilityChanged') {
            this._isSidebarOpen = !!message.isOpen;
        }
    }

    static _nextGuacID = 0;

    _drawAndScaleCursor() {
        if (!this._cursorImageBitmap) {
            return;
        }
        const dpr = this.useCssScaling ? 1 : (window.devicePixelRatio || 1);
        const img = this._cursorImageBitmap;
        this.cursorDiv.width = img.width;
        this.cursorDiv.height = img.height;
        this.cursorDiv.style.width = `${img.width / dpr}px`;
        this.cursorDiv.style.height = `${img.height / dpr}px`;
        this.cursorImg.clearRect(0, 0, img.width, img.height);
        this.cursorImg.drawImage(img, 0, 0);
        this.cursorHotspot.x = this._rawHotspotX / dpr;
        this.cursorHotspot.y = this._rawHotspotY / dpr;
        this._updateCursorPosition(this._latestMouseX, this._latestMouseY);
    }

    _handleOutsideClick(event) {
        if (!this.use_browser_cursors && !this.element.contains(event.target)) {
            this.cursorDiv.style.display = 'none';
        }
    }
    _updateCursorPosition(clientX, clientY) {
        if (this.cursorDiv.style.display !== 'none') {
            const newX = clientX - this.cursorHotspot.x;
            const newY = clientY - this.cursorHotspot.y;
            this.cursorDiv.style.transform = `translate(${newX}px, ${newY}px)`;
        }
    }

    _updateBrowserCursor() {
        if (!this._cursorBase64Data) {
            this.element.style.setProperty('cursor', 'none', 'important');
            return;
        }
        const cursorDataUrl = `data:image/png;base64,${this._cursorBase64Data}`;
        this.element.style.setProperty('cursor', `url("${cursorDataUrl}") ${this._rawHotspotX} ${this._rawHotspotY}, default`, 'important');
    }

    async updateServerCursor(cursorData) {
        if (!cursorData.curdata ||
            parseInt(cursorData.handle, 10) === 0 ||
            this._trackpadMode)
        {
            this._cursorImageBitmap = null;
            this._cursorBase64Data = null;
            this.cursorDiv.style.display = 'none';
            if (this.use_browser_cursors) {
                this.element.style.setProperty('cursor', 'none', 'important');
            }
            return;
        }
        this._rawHotspotX = parseInt(cursorData.hotx) || 0;
        this._rawHotspotY = parseInt(cursorData.hoty) || 0;
        this._cursorBase64Data = cursorData.curdata;
        if (!this.inputAttached) {
            this.cursorDiv.style.display = 'none';
            this.element.style.cursor = 'auto';
            return;
        }
        if (this.use_browser_cursors) {
            this.cursorDiv.style.display = 'none';
            this._updateBrowserCursor();
        } else {
            const blob = await (await fetch(`data:image/png;base64,${this._cursorBase64Data}`)).blob();
            this._cursorImageBitmap = await createImageBitmap(blob);
            this.element.style.setProperty('cursor', 'none', 'important');
            this.cursorDiv.style.display = 'block';
            this._drawAndScaleCursor();
        }
    }

    setSynth(isSynth) {
        console.log(`Input: Synthetic mode ${isSynth ? 'enabled' : 'disabled'}.`);
        this._isSynth = isSynth;
    }

    updateCssScaling(newUseCssScalingValue) {
        if (this.useCssScaling !== newUseCssScalingValue) {
            console.log(`Input: Updating useCssScaling from ${this.useCssScaling} to ${newUseCssScalingValue}`);
            this.useCssScaling = newUseCssScalingValue;
            this._windowMath();
            this._drawAndScaleCursor();
        }
    }

    _sendKeyEvent(keysym, code, down) {
        if (keysym === null) return;
        let finalKeysymToSend = keysym;
        if (NumpadTranslations_NumLockOn.hasOwnProperty(keysym)) {
            finalKeysymToSend = NumpadTranslations_NumLockOn[keysym];
        } else if (NumpadTranslations_NumLockOff.hasOwnProperty(keysym)) {
            finalKeysymToSend = NumpadTranslations_NumLockOff[keysym];
        }
        if (down) {
            this._keyDownList[code] = finalKeysymToSend;
        } else {
            if (!(code in this._keyDownList)) {
                return;
            }
            finalKeysymToSend = this._keyDownList[code];
            delete this._keyDownList[code];
        }
        
        this.send((down ? "kd," : "ku,") + finalKeysymToSend);
    }

    resetKeyboard() {
        for (const code in this._keyDownList) {
            this._sendKeyEvent(this._keyDownList[code], code, false);
        }
        this._keyDownList = {};
    }

    _guac_markEvent(e) {
        if (e[this._EVENT_MARKER]) {
            return false;
        }
        e[this._EVENT_MARKER] = true;
        return true;
    }

    _handleKeyDown(event) {
        if (this._targetHasClass(event.target, WHITELIST_CLASS)) return;
        if (!this._guac_markEvent(event)) return;
        if (this.isComposing || event.isComposing || event.keyCode === 229) {
            _stopEvent(event);
            return;
        }

        if (!this._isSynth) {
            for (const code in this._keyDownList) {
                const keysym = this._keyDownList[code];
                if ((code === 'ControlLeft' || code === 'ControlRight') && !event.ctrlKey) {
                    this._sendKeyEvent(keysym, code, false);
                }
                if ((code === 'MetaLeft' || code === 'MetaRight') && !event.metaKey) {
                    this._sendKeyEvent(keysym, code, false);
                }
                if ((code === 'AltLeft' || code === 'AltRight') && !event.altKey) {
                    this._sendKeyEvent(keysym, code, false);
                }
                if ((code === 'ShiftLeft' || code === 'ShiftRight') && !event.shiftKey) {
                    this._sendKeyEvent(keysym, code, false);
                }
            }
        }

        if (event.code === 'KeyM' && event.ctrlKey && event.shiftKey) {
            if (document.fullscreenElement === null && this.onmenuhotkey !== null) {
                this.onmenuhotkey();
                _stopEvent(event);
                return;
            }
        }
        if (event.code === 'KeyF' && event.ctrlKey && event.shiftKey) {
            if (document.fullscreenElement === null && this.onfullscreenhotkey !== null) {
                this.onfullscreenhotkey();
                _stopEvent(event);
                return;
            }
        }

        const code = KeyboardUtil.getKeyCode(event);
        let keysym = KeyboardUtil.getKeysym(event);

        if (this._altGrArmed) {
            this._altGrArmed = false;
            clearTimeout(this._altGrTimeout);
            if ((code === "AltRight") && ((event.timeStamp - this._altGrCtrlTime) < 50)) {
                keysym = KeyTable.XK_ISO_Level3_Shift;
            } else {
                this._sendKeyEvent(KeyTable.XK_Control_L, "ControlLeft", true);
            }
        }

        if (code === 'Unidentified' && keysym) {
            this._sendKeyEvent(keysym, code, true);
            this._sendKeyEvent(keysym, code, false);
            _stopEvent(event);
            return;
        }

        if (browser.isMac() && code !== "MetaLeft" && code !== "MetaRight" &&
            event.metaKey && !event.ctrlKey && !event.altKey) {
            if (this._keyDownList["MetaLeft"] || this._keyDownList["MetaRight"]) {
                console.log(`macOS: Cmd+key detected for code '${code}'. Remapping Cmd to Ctrl.`);
                if (this._keyDownList["MetaLeft"]) {
                    this._sendKeyEvent(this._keyDownList["MetaLeft"], "MetaLeft", false);
                }
                if (this._keyDownList["MetaRight"]) {
                    this._sendKeyEvent(this._keyDownList["MetaRight"], "MetaRight", false);
                }
                this._sendKeyEvent(KeyTable.XK_Control_L, "ControlLeft", true);
                this._macCmdSwapped = true;
            }
        }

        if (browser.isMac() || browser.isIOS()) {
            switch (keysym) {
                case KeyTable.XK_Super_L: keysym = KeyTable.XK_Alt_L; break;
                case KeyTable.XK_Super_R: keysym = KeyTable.XK_Super_L; break; // Should be Alt_R, but X11 convention...
                case KeyTable.XK_Alt_L: keysym = KeyTable.XK_Mode_switch; break;
                case KeyTable.XK_Alt_R: keysym = KeyTable.XK_ISO_Level3_Shift; break;
            }
        }

        if (code in this._keyDownList) { // Key already pressed
            keysym = this._keyDownList[code];
        }

        if ((browser.isMac() || browser.isIOS()) && (code === 'CapsLock')) {
            this._sendKeyEvent(KeyTable.XK_Caps_Lock, 'CapsLock', true);
            this._sendKeyEvent(KeyTable.XK_Caps_Lock, 'CapsLock', false);
            _stopEvent(event);
            return;
        }

        const jpBadKeys = [
            KeyTable.XK_Zenkaku_Hankaku, KeyTable.XK_Eisu_toggle,
            KeyTable.XK_Katakana, KeyTable.XK_Hiragana, KeyTable.XK_Romaji
        ];
        if (browser.isWindows() && jpBadKeys.includes(keysym)) {
            this._sendKeyEvent(keysym, code, true);
            this._sendKeyEvent(keysym, code, false);
            _stopEvent(event);
            return;
        }

        _stopEvent(event);

        if ((code === "ControlLeft") && browser.isWindows() && !(code in this._keyDownList)) {
            this._altGrArmed = true;
            this._altGrCtrlTime = event.timeStamp;
            this._altGrTimeout = setTimeout(() => {
                this._altGrArmed = false;
                this._sendKeyEvent(KeyTable.XK_Control_L, "ControlLeft", true);
            }, 100);
            return;
        }
        this._sendKeyEvent(keysym, code, true);
    }

    _handleKeyPress(event) {
        if (this._targetHasClass(event.target, WHITELIST_CLASS)) return;
        if (!this._guac_markEvent(event)) return;
    }

    _handleKeyUp(event) {
        if (this._targetHasClass(event.target, WHITELIST_CLASS)) return;
        if (!this._guac_markEvent(event)) return;
        
        _stopEvent(event);

        const code = KeyboardUtil.getKeyCode(event);

        if (browser.isMac() && (code === 'MetaLeft' || code === 'MetaRight')) {
            console.log(`macOS: Command key ('${code}') released. Cleaning up potentially stuck keys.`);

            const pressedCodes = Object.keys(this._keyDownList);
            for (const pressedCode of pressedCodes) {
                // Ignore the meta key that is currently being released, and other modifiers.
                if (pressedCode === 'ShiftLeft' || pressedCode === 'ShiftRight' ||
                    pressedCode === 'ControlLeft' || pressedCode === 'ControlRight' ||
                    pressedCode === 'AltLeft' || pressedCode === 'AltRight' ||
                    pressedCode === 'MetaLeft' || pressedCode === 'MetaRight') {
                    continue;
                }

                console.log(`macOS: Force-releasing stuck key: ${pressedCode}`);
                this._sendKeyEvent(this._keyDownList[pressedCode], pressedCode, false);
            }
            
            if (this._macCmdSwapped) {
                console.log("macOS: Releasing the swapped virtual Ctrl key.");
                if ('ControlLeft' in this._keyDownList) {
                    this._sendKeyEvent(this._keyDownList['ControlLeft'], 'ControlLeft', false);
                }
                this._macCmdSwapped = false;
            }
        }

        if (this._altGrArmed) { // Abort AltGr if keyup is not AltRight
            this._altGrArmed = false;
            clearTimeout(this._altGrTimeout);
            this._sendKeyEvent(KeyTable.XK_Control_L, "ControlLeft", true);
        }

        if ((browser.isMac() || browser.isIOS()) && (code === 'CapsLock')) {
            return;
        }

        const keysym = this._keyDownList[code];
        this._sendKeyEvent(keysym, code, false);

        if (browser.isWindows() && ((code === 'ShiftLeft') || (code === 'ShiftRight'))) {
            if ('ShiftRight' in this._keyDownList) {
                this._sendKeyEvent(this._keyDownList['ShiftRight'], 'ShiftRight', false);
            }
            if ('ShiftLeft' in this._keyDownList) {
                this._sendKeyEvent(this._keyDownList['ShiftLeft'], 'ShiftLeft', false);
            }
        }
    }

    _updateCompositionText(newText) {
        const oldValue = this.compositionString;
        const newValue = newText || "";

        let diff_start = 0;
        while (diff_start < oldValue.length && diff_start < newValue.length && oldValue[diff_start] === newValue[diff_start]) {
            diff_start++;
        }

        const backspaces = oldValue.length - diff_start;
        for (let i = 0; i < backspaces; i++) {
            this._sendKeyEvent(KeyTable.XK_BackSpace, "Backspace", true);
            this._sendKeyEvent(KeyTable.XK_BackSpace, "Backspace", false);
        }

        const newChars = newValue.substring(diff_start);
        for (let i = 0; i < newChars.length; i++) {
            const keysym = Keysyms.lookup(newChars.charCodeAt(i));
            if (keysym) {
                this._sendKeyEvent(keysym, 'Unidentified', true);
                this._sendKeyEvent(keysym, 'Unidentified', false);
            }
        }

        this.compositionString = newValue;
    }

    _compositionStart(event) {
        if (!this._guac_markEvent(event)) return;
        this.isComposing = true;
        this.compositionString = "";
    }

    _compositionUpdate(event) {
        if (!this._guac_markEvent(event)) return;
        if (!this.isComposing) return;
        this._updateCompositionText(event.data);
    }

    _compositionEnd(event) {
        if (!this._guac_markEvent(event)) return;
        if (!this.isComposing) return;
        if (browser.isLinux()) {
            this.isComposing = false;
            this.compositionString = "";
            return;
        }
        this._updateCompositionText(event.data);
        this.isComposing = false;
        this.compositionString = "";
    }

    _handleTextInput(event) {
        if (!event.data) return;

        const text = event.data;
        for (let i = 0; i < text.length; i++) {
            const codepoint = text.charCodeAt(i);
            const keysym = Keysyms.lookup(codepoint);
            if (keysym) {
                this._sendKeyEvent(keysym, 'Unidentified', true);
                this._sendKeyEvent(keysym, 'Unidentified', false);
            }
        }
    }

    _handleMobileInput(event) {
        const text = event.target.value;
        if (!text) {
            return;
        }
        for (let i = 0; i < text.length; i++) {
            const char = text[i];
            const isUpperCase = char >= 'A' && char <= 'Z';
            if (isUpperCase) {
                this.send("kd," + KeyTable.XK_Shift_L);
                const lowerChar = char.toLowerCase();
                const letterKeysym = Keysyms.lookup(lowerChar.charCodeAt(0));
                if (letterKeysym) {
                    this.send("kd," + letterKeysym);
                    this.send("ku," + letterKeysym);
                }
                this.send("ku," + KeyTable.XK_Shift_L);
            } else {
                const keysym = Keysyms.lookup(char.charCodeAt(0));
                if (keysym) {
                    this.send("kd," + keysym);
                    this.send("ku," + keysym);
                }
            }
        }
        event.target.value = '';
    }

    _mouseButtonMovement(event) {
        if (this.buttonMask === 0 && event.target !== this.element) {
            return;
        }
        if (this.inputAttached && !this.use_browser_cursors) {
            this.cursorDiv.style.display = 'block';
            this.element.style.setProperty('cursor', 'none', 'important');
        }
        let visualClientX = event.clientX;
        let visualClientY = event.clientY;
        if (event.getPredictedEvents && typeof event.getPredictedEvents === 'function') {
            const predictedEvents = event.getPredictedEvents();
            if (predictedEvents.length > 0) {
                const lastPredictedEvent = predictedEvents[predictedEvents.length - 1];
                visualClientX = lastPredictedEvent.clientX;
                visualClientY = lastPredictedEvent.clientY;
            }
        }
        if (this.inputAttached && !this.use_browser_cursors) {
            this._updateCursorPosition(visualClientX, visualClientY);
        }
        this._latestMouseX = visualClientX;
        this._latestMouseY = visualClientY;
        if (this._trackpadMode) return;
        const client_dpr = window.devicePixelRatio || 1;
        const dpr_for_input_coords = this.useCssScaling ? 1 : client_dpr;
        const down = (event.type === 'mousedown' || event.type === 'pointerdown' ? 1 : 0);
        var mtype = "m";
        let canvas = document.getElementById('videoCanvas');
        if (event.type === 'mousedown' || event.type === 'mouseup' || event.type === 'pointerdown' || event.type === 'pointerup' || event.type === 'pointercancel') {
            if (event.button === 3) {
                event.preventDefault();
            } else if (event.button === 4) {
                event.preventDefault();
            }
        }
        if (down && event.button === 0 && event.ctrlKey && event.shiftKey) {
            const targetElement = event.target.requestPointerLock ? event.target : this.element;
            targetElement.requestPointerLock().catch(err => console.error("Pointer lock failed:", err));
            event.preventDefault();
            return;
        }
        if (document.pointerLockElement === this.element || document.pointerLockElement === canvas) {
            mtype = "m2";
            let movementX_logical = event.movementX || 0;
            let movementY_logical = event.movementY || 0;
            this.x = Math.round(movementX_logical * dpr_for_input_coords);
            this.y = Math.round(movementY_logical * dpr_for_input_coords);

        } else if (event.type === 'mousemove' || event.type === 'pointermove') {
             if ((window.is_manual_resolution_mode || this.isSharedMode) && canvas) {
                const canvasRect = canvas.getBoundingClientRect(); // CSS logical size
                if (canvasRect.width > 0 && canvasRect.height > 0 && canvas.width > 0 && canvas.height > 0) {
                    const mouseX_on_canvas_logical_css = event.clientX - canvasRect.left;
                    const mouseY_on_canvas_logical_css = event.clientY - canvasRect.top;
                    const scaleX = canvas.width / canvasRect.width;
                    const scaleY = canvas.height / canvasRect.height;
                    let coordX = mouseX_on_canvas_logical_css * scaleX;
                    let coordY = mouseY_on_canvas_logical_css * scaleY;
                    this.x = Math.max(0, Math.min(canvas.width, Math.round(coordX)));
                    this.y = Math.max(0, Math.min(canvas.height, Math.round(coordY)));
                } else {
                    this.x = 0; this.y = 0;
                }
            } else { // Auto resolution mode (non-manual)
                if (!this.m) {
                    this._windowMath();
                }
                if (this.m) {
                    let logicalX_on_element = this._clientToServerX(event.clientX);
                    let logicalY_on_element = this._clientToServerY(event.clientY);
                    this.x = Math.round(logicalX_on_element * dpr_for_input_coords);
                    this.y = Math.round(logicalY_on_element * dpr_for_input_coords);
                } else {
                    this.x = 0; this.y = 0;
                }
            }
        }
        if (event.type === 'mousedown' || event.type === 'mouseup') {
            var mask = 1 << event.button;
            if (down) {
                this.buttonMask |= mask;
            } else {
                this.buttonMask &= ~mask;
            }
        }
        var toks = [ mtype, this.x, this.y, this.buttonMask, 0 ];
        this.send(toks.join(","));
    }

    _handlePointerDown(event) {
        if (event.pointerType !== 'pen') {
            return;
        }
        event.preventDefault();
        this._mouseButtonMovement(event);
    }

    _handlePointerMove(event) {
        if (event.pointerType !== 'pen') {
           return;
        }
        this._mouseButtonMovement(event);
    }
 
    _handlePointerUp(event) {
        if (event.pointerType !== 'pen') {
            return;
        }
        this._mouseButtonMovement(event);
    }

    _handleTrackpadEvent(event) {
        if (this._targetHasClass(event.target, WHITELIST_CLASS)) return;
        event.preventDefault();
        event.stopPropagation();

        const now = Date.now();
        const dpr = this.useCssScaling ? 1 : (window.devicePixelRatio || 1);
        const TAP_AND_HOLD_THRESHOLD = 300;

        const type = event.type;
        const changedTouches = event.changedTouches;

        if (type === 'touchstart') {
            if (this._trackpadTapTimeout) {
                clearTimeout(this._trackpadTapTimeout);
                this._trackpadTapTimeout = null;
            }

            for (const touch of changedTouches) {
                this._trackpadTouches.set(touch.identifier, {
                    id: touch.identifier,
                    startX: touch.clientX, startY: touch.clientY,
                    lastX: touch.clientX, lastY: touch.clientY,
                    moved: false
                });
            }

            const touchCount = this._trackpadTouches.size;

            if (touchCount === 1) {
                if ((now - this._trackpadLastTapTime) < TAP_AND_HOLD_THRESHOLD) {
                    this._trackpadGestureMode = 'dragging';
                    this.buttonMask |= 1;
                    this.send(`m2,0,0,${this.buttonMask},0`);
                    this._trackpadLastTapTime = 0;
                } else {
                    this._trackpadGestureMode = 'moving';
                }
            }
            else if (touchCount === 2) {
                this._trackpadGestureMode = 'scrolling';
                this._trackpadLastTapTime = 0;
                const touches = Array.from(this._trackpadTouches.values());
                this._trackpadLastScrollCentroid = {
                    x: (touches[0].lastX + touches[1].lastX) / 2,
                    y: (touches[0].lastY + touches[1].lastY) / 2
                };
            }
        }
        else if (type === 'touchmove') {
            let hasAnyFingerMovedBeyondThreshold = false;
            for (const touch of this._trackpadTouches.values()) {
                if (!touch.moved) {
                    const currentTouch = Array.from(changedTouches).find(t => t.identifier === touch.id) || touch;
                    if (currentTouch) {
                        const dx = currentTouch.clientX - touch.startX;
                        const dy = currentTouch.clientY - touch.startY;
                        if (dx * dx + dy * dy > this._TAP_THRESHOLD_DISTANCE_SQ) {
                            touch.moved = true;
                        }
                    }
                }
                if (touch.moved) {
                    hasAnyFingerMovedBeyondThreshold = true;
                }
            }

            if (hasAnyFingerMovedBeyondThreshold) {
                this._trackpadLastTapTime = 0;
            }

            if (this._trackpadGestureMode === 'moving' || this._trackpadGestureMode === 'dragging') {
                const touchData = this._trackpadTouches.values().next().value;
                if (touchData) {
                    const changedTouch = Array.from(changedTouches).find(t => t.identifier === touchData.id);
                    if (changedTouch) {
                        const deltaX = (changedTouch.clientX - touchData.lastX) * dpr;
                        const deltaY = (changedTouch.clientY - touchData.lastY) * dpr;
                        if (Math.abs(deltaX) >= 0.5 || Math.abs(deltaY) >= 0.5) {
                            this.send(`m2,${Math.round(deltaX)},${Math.round(deltaY)},${this.buttonMask},0`);
                        }
                        touchData.lastX = changedTouch.clientX;
                        touchData.lastY = changedTouch.clientY;
                    }
                }
            } else if (this._trackpadGestureMode === 'scrolling') {
                const touches = Array.from(this._trackpadTouches.values());
                if (touches.length === 2) {
                    for (const changed of changedTouches) {
                        const data = this._trackpadTouches.get(changed.identifier);
                        if (data) { data.lastX = changed.clientX; data.lastY = changed.clientY; }
                    }
                    const curr_avg_x = (touches[0].lastX + touches[1].lastX) / 2;
                    const curr_avg_y = (touches[0].lastY + touches[1].lastY) / 2;
                    if (this._trackpadLastScrollCentroid) {
                        const deltaX = curr_avg_x - this._trackpadLastScrollCentroid.x;
                        const deltaY = curr_avg_y - this._trackpadLastScrollCentroid.y;
                        const SCROLL_THRESHOLD = 2;
                        if (Math.abs(deltaY) > SCROLL_THRESHOLD) this._triggerMouseWheel(deltaY < 0 ? 'down' : 'up', 1);
                        if (Math.abs(deltaX) > SCROLL_THRESHOLD) this._triggerHorizontalMouseWheel(deltaX < 0 ? 'left' : 'right', 1);
                    }
                    this._trackpadLastScrollCentroid = { x: curr_avg_x, y: curr_avg_y };
                }
            }
        }
        else if (type === 'touchend' || type === 'touchcancel') {
            const touchCountBeforeEnd = this._trackpadTouches.size;
            const wasTap = !Array.from(this._trackpadTouches.values()).some(t => t.moved);

            if (touchCountBeforeEnd === 2 && wasTap) {
                this.buttonMask |= (1 << 2); this.send(`m2,0,0,${this.buttonMask},0`);
                setTimeout(() => { this.buttonMask &= ~(1 << 2); this.send(`m2,0,0,${this.buttonMask},0`); }, 50);
                this._trackpadGestureMode = 'completed';
                this._trackpadLastTapTime = 0;
            }
            else if (touchCountBeforeEnd === 1 && wasTap && this._trackpadGestureMode !== 'completed' && this._trackpadGestureMode !== 'dragging') {
                this._trackpadLastTapTime = now;
                this._trackpadTapTimeout = setTimeout(() => {
                    this.buttonMask |= 1; this.send(`m2,0,0,${this.buttonMask},0`);
                    setTimeout(() => { this.buttonMask &= ~1; this.send(`m2,0,0,${this.buttonMask},0`); }, 50);
                }, 200);
            }

            for (const touch of changedTouches) {
                this._trackpadTouches.delete(touch.identifier);
            }

            if (this._trackpadTouches.size === 0) {
                if (this._trackpadGestureMode === 'dragging') {
                    this.buttonMask &= ~1;
                    this.send(`m2,0,0,${this.buttonMask},0`);
                }
                this._trackpadGestureMode = null;
                this._trackpadLastScrollCentroid = null;
            }
        }
    }

    _calculateTouchCoordinates(touchPoint) {
        this._updateCursorPosition(touchPoint.clientX, touchPoint.clientY);
        this._latestMouseX = touchPoint.clientX;
        this._latestMouseY = touchPoint.clientY;
        const client_dpr = window.devicePixelRatio || 1; // Actual client DPR
        const dpr_for_input_coords = this.useCssScaling ? 1 : client_dpr;
        let canvas = document.getElementById('videoCanvas');

        if ((window.is_manual_resolution_mode || this.isSharedMode) && canvas) {
            const canvasRect = canvas.getBoundingClientRect(); // CSS logical size
            if (canvasRect.width > 0 && canvasRect.height > 0 && canvas.width > 0 && canvas.height > 0) {
                const touchX_on_canvas_logical_css = touchPoint.clientX - canvasRect.left;
                const touchY_on_canvas_logical_css = touchPoint.clientY - canvasRect.top;

                const scaleX = canvas.width / canvasRect.width; // buffer / CSS
                const scaleY = canvas.height / canvasRect.height; // buffer / CSS

                let coordX = touchX_on_canvas_logical_css * scaleX;
                let coordY = touchY_on_canvas_logical_css * scaleY;

                this.x = Math.max(0, Math.min(canvas.width, Math.round(coordX)));
                this.y = Math.max(0, Math.min(canvas.height, Math.round(coordY)));
            } else {
                this.x = 0; this.y = 0;
            }
        } else { // Auto resolution mode (non-manual)
            if (!this.m) this._windowMath();
            if (this.m) {
                let logicalX_on_element = this._clientToServerX(touchPoint.clientX);
                let logicalY_on_element = this._clientToServerY(touchPoint.clientY);
                this.x = Math.round(logicalX_on_element * dpr_for_input_coords);
                this.y = Math.round(logicalY_on_element * dpr_for_input_coords);
            } else {
                this.x = Math.round(touchPoint.clientX * dpr_for_input_coords);
                this.y = Math.round(touchPoint.clientY * dpr_for_input_coords);
            }
        }
    }

    _sendMouseState() {
        const mtype = (document.pointerLockElement === this.element || this.mouseRelative) ? "m2" : "m";
        const toks = [ mtype, this.x, this.y, this.buttonMask, 0 ];
        this.send(toks.join(","));
    }

    setTrackpadMode(enabled) {
        const newMode = !!enabled;
        if (this._trackpadMode === newMode) {
            return;
        }

        console.log(`Input: Trackpad mode ${newMode ? 'enabled' : 'disabled'}.`);
        this._trackpadMode = newMode;

        this._activeTouches.clear();
        this._activeTouchIdentifier = null;
        this._isTwoFingerGesture = false;
        this._touchScrollLastCentroid = null;

        if (this._longPressTimer) {
            clearTimeout(this._longPressTimer);
            this._longPressTimer = null;
            this._longPressTouchIdentifier = null;
        }

        if (this.buttonMask !== 0) {
            this.buttonMask = 0;
            this._sendMouseState();
        }

        if (this._trackpadMode || this.use_browser_cursors) {
            this.element.style.setProperty('cursor', 'none', 'important');
            this.element.style.cursor = 'default';
        } else {
            this.element.style.setProperty('cursor', 'none', 'important');
            this.cursorDiv.style.display = 'none';
        }
    }

    async setUseBrowserCursors(enabled) {
        const newMode = !!enabled;
        if (this.use_browser_cursors === newMode) {
            return;
        }
        console.log(`Input: Use browser cursors ${newMode ? 'enabled' : 'disabled'}.`);
        this.use_browser_cursors = newMode;
        if (this._trackpadMode) {
            this.cursorDiv.style.display = 'none';
            this.element.style.setProperty('cursor', 'none', 'important');
        } else if (this.use_browser_cursors) {
            this.cursorDiv.style.display = 'none';
            this._updateBrowserCursor();
        } else {
            this.element.style.setProperty('cursor', 'none', 'important');
            if (this._cursorBase64Data && !this._cursorImageBitmap) {
                const blob = await (await fetch(`data:image/png;base64,${this._cursorBase64Data}`)).blob();
                this._cursorImageBitmap = await createImageBitmap(blob);
            }
            if (this._cursorImageBitmap) {
                this.cursorDiv.style.display = 'block';
                this._drawAndScaleCursor();
            } else {
                this.cursorDiv.style.display = 'none';
            }
        }
    }

    _handleTouchEvent(event) {
        if (this._trackpadMode) {
            this._handleTrackpadEvent(event);
            return;
        }
        if (this._targetHasClass(event.target, WHITELIST_CLASS)) return;
        if (!this._guac_markEvent(event)) return;
        const type = event.type;
        const now = Date.now();
        let preventDefault = false;
        const LONG_PRESS_DURATION = 750;
        let activeTouchMoved = false;
        const LONG_PRESS_MAX_MOVEMENT_SQ = 15 * 15;
        const TAP_THRESHOLD_DISTANCE_SQ_LOGICAL = this._TAP_THRESHOLD_DISTANCE_SQ;

        if (type === 'touchstart') {
            if (!this.use_browser_cursors) {
                this.cursorDiv.style.display = 'block';
            }
            for (let i = 0; i < event.changedTouches.length; i++) {
                const touch = event.changedTouches[i];
                if (!this._activeTouches.has(touch.identifier)) {
                    this._activeTouches.set(touch.identifier, {
                        startX: touch.clientX, startY: touch.clientY,
                        currentX: touch.clientX, currentY: touch.clientY,
                        startTime: now, identifier: touch.identifier,
                        longPressCompleted: false
                    });
                    if (i === 0) {
                        this._calculateTouchCoordinates(touch);
                    }
                }
            }
            const touchCount = this._activeTouches.size;
            if (touchCount === 1 && !this._isTwoFingerGesture) {
                preventDefault = true;
                const [singleTouchID] = this._activeTouches.keys();
                const touchData = this._activeTouches.get(singleTouchID);
                const currentTouchPoint = { clientX: touchData.currentX, clientY: touchData.currentY };
                this._calculateTouchCoordinates(currentTouchPoint);
                const physicalXAtPressStart = this.x;
                const physicalYAtPressStart = this.y;
                if (touchData && !touchData.longPressCompleted) {
                    this._longPressTouchIdentifier = singleTouchID;
                    if (this._longPressTimer) clearTimeout(this._longPressTimer);
                    this._longPressTimer = setTimeout(() => {
                        const currentActiveTouchData = this._activeTouches.get(this._longPressTouchIdentifier);
                        if (currentActiveTouchData && this._activeTouches.size === 1 &&
                            this._longPressTouchIdentifier === currentActiveTouchData.identifier &&
                            !this._isTwoFingerGesture && this._activeTouchIdentifier === null &&
                            !currentActiveTouchData.longPressCompleted) {
                            const dx = currentActiveTouchData.currentX - currentActiveTouchData.startX;
                            const dy = currentActiveTouchData.currentY - currentActiveTouchData.startY;
                            const distSq = dx * dx + dy * dy;
                            if (distSq < LONG_PRESS_MAX_MOVEMENT_SQ) {
                                currentActiveTouchData.longPressCompleted = true;
                                this.x = physicalXAtPressStart;
                                this.y = physicalYAtPressStart;
                                this.buttonMask |= (1 << 2);
                                this._sendMouseState();
                                setTimeout(() => {
                                    if ((this.buttonMask & (1 << 2)) !== 0) {
                                        this.buttonMask &= ~(1 << 2);
                                        this._sendMouseState();
                                    }
                                }, 50);
                            }
                        }
                        this._longPressTimer = null;
                    }, LONG_PRESS_DURATION);
                }
            } else {
                if (this._longPressTimer) { clearTimeout(this._longPressTimer); this._longPressTimer = null; }
                if (touchCount === 2) {
                    if (!this.use_browser_cursors) {
                        this.cursorDiv.style.visibility = 'hidden';
                    }
                    this._isTwoFingerGesture = true; this._activeTouchIdentifier = null;
                    const touches = Array.from(this._activeTouches.values());
                    this._touchScrollLastCentroid = {
                        x: (touches[0].currentX + touches[1].currentX) / 2,
                        y: (touches[0].currentY + touches[1].currentY) / 2
                    };
                    if ((this.buttonMask & 1) === 1) this.buttonMask &= ~1;
                    preventDefault = true;
                } else if (touchCount > 2) {
                    if (this._isTwoFingerGesture) this._isTwoFingerGesture = false;
                    if (this._activeTouchIdentifier !== null) {
                        this.buttonMask &= ~1; this._sendMouseState(); this._activeTouchIdentifier = null;
                    }
                }
                if (touchCount !== 1) { this._longPressTouchIdentifier = null; }
            }
        } else if (type === 'touchmove') {
            for (let i = 0; i < event.changedTouches.length; i++) {
                const touch = event.changedTouches[i];
                const touchData = this._activeTouches.get(touch.identifier);
                if (touchData) {
                    touchData.currentX = touch.clientX; touchData.currentY = touch.clientY;
                    if (this._longPressTimer && touch.identifier === this._longPressTouchIdentifier) {
                        const dx = touchData.currentX - touchData.startX;
                        const dy = touchData.currentY - touchData.startY;
                        const distSq = dx * dx + dy * dy;
                        if (distSq >= LONG_PRESS_MAX_MOVEMENT_SQ) {
                            clearTimeout(this._longPressTimer); this._longPressTimer = null;
                        }
                    }
                }
            }
        }
        if (this._isTwoFingerGesture && this._activeTouches.size === 2) {
            preventDefault = true;
            const touches = Array.from(this._activeTouches.values());
            const curr_avg_x = (touches[0].currentX + touches[1].currentX) / 2;
            const curr_avg_y = (touches[0].currentY + touches[1].currentY) / 2;
            if (this._touchScrollLastCentroid) {
                const deltaX = curr_avg_x - this._touchScrollLastCentroid.x;
                const deltaY = curr_avg_y - this._touchScrollLastCentroid.y;
                const SCROLL_THRESHOLD = 2;
                if (Math.abs(deltaY) > SCROLL_THRESHOLD) this._triggerMouseWheel(deltaY < 0 ? 'down' : 'up', 1);
                if (Math.abs(deltaX) > SCROLL_THRESHOLD) this._triggerHorizontalMouseWheel(deltaX < 0 ? 'left' : 'right', 1);
            }
            this._touchScrollLastCentroid = { x: curr_avg_x, y: curr_avg_y };
        } else if (this._activeTouches.size === 1) {
            const [singleTouchID] = this._activeTouches.keys();
            const touchData = this._activeTouches.get(singleTouchID);
            if (this._activeTouchIdentifier === singleTouchID) {
                this._calculateTouchCoordinates({ clientX: touchData.currentX, clientY: touchData.currentY }); this._sendMouseState();
                activeTouchMoved = true; preventDefault = true;
            } else if (this._activeTouchIdentifier === null && !touchData.longPressCompleted) {
                const dx = touchData.currentX - touchData.startX;
                const dy = touchData.currentY - touchData.startY;
                const distSq = dx * dx + dy * dy;
                if (distSq >= TAP_THRESHOLD_DISTANCE_SQ_LOGICAL) {
                    if (this._longPressTimer && singleTouchID === this._longPressTouchIdentifier) { clearTimeout(this._longPressTimer); this._longPressTimer = null; }
                    this._activeTouchIdentifier = singleTouchID;
                    this._calculateTouchCoordinates({ clientX: touchData.currentX, clientY: touchData.currentY });
                    this.buttonMask |= 1; this._sendMouseState();
                    activeTouchMoved = true; preventDefault = true;
                } else { preventDefault = true; }
            }
        }
        if (this._activeTouchIdentifier !== null && !activeTouchMoved && this._activeTouches.size > 0) {
             preventDefault = true;
        } else if (type === 'touchend' || type === 'touchcancel') {
            const endedTouches = event.changedTouches;
            let swipeDetected = false;
            for (let i = 0; i < endedTouches.length; i++) {
                const endedTouch = endedTouches[i];
                const identifier = endedTouch.identifier;
                const startData = this._activeTouches.get(identifier);
                if (!startData) continue;
                if (this._longPressTimer && identifier === this._longPressTouchIdentifier) {
                    clearTimeout(this._longPressTimer); this._longPressTimer = null;
                }
                if (startData.longPressCompleted) {
                    this._activeTouches.delete(identifier);
                    if (identifier === this._longPressTouchIdentifier) this._longPressTouchIdentifier = null;
                    preventDefault = true; continue;
                }
                startData.currentX = endedTouch.clientX; startData.currentY = endedTouch.clientY;
                const duration = now - startData.startTime;
                const deltaX = startData.currentX - startData.startX;
                const deltaY = startData.currentY - startData.startY;
                const deltaDistSq = deltaX * deltaX + deltaY * deltaY;
                if (this._isTwoFingerGesture) {
                    // Scrolling is handled externally
                } else if (!swipeDetected && this._activeTouchIdentifier === null && this._activeTouches.size === 1 && this._activeTouches.has(identifier)) {
                    if (duration < this._TAP_MAX_DURATION && deltaDistSq < TAP_THRESHOLD_DISTANCE_SQ_LOGICAL) {
                        this._calculateTouchCoordinates(endedTouch); this.buttonMask |= 1; this._sendMouseState(); preventDefault = true;
                        setTimeout(() => { this.buttonMask &= ~1; this._sendMouseState(); }, 10);
                    }
                } else if (!swipeDetected && identifier === this._activeTouchIdentifier) {
                    this._calculateTouchCoordinates(endedTouch); this.buttonMask &= ~1; this._sendMouseState();
                    this._activeTouchIdentifier = null; preventDefault = true;
                }
                this._activeTouches.delete(identifier);
                if (identifier === this._longPressTouchIdentifier) this._longPressTouchIdentifier = null;
            }
            if (!swipeDetected) {
                const remainingTouchCount = this._activeTouches.size;
                if (this._isTwoFingerGesture && remainingTouchCount < 2) {
                    if (!this._trackpadMode && !this.use_browser_cursors) {
                        this.cursorDiv.style.visibility = 'visible';
                    }
                    this._isTwoFingerGesture = false;
                    this._touchScrollLastCentroid = null;
                }
                if (remainingTouchCount === 0) {
                    this._activeTouchIdentifier = null; this._isTwoFingerGesture = false;
                    this._touchScrollLastCentroid = null;
                    if (this._longPressTimer) { clearTimeout(this._longPressTimer); this._longPressTimer = null; }
                    this._longPressTouchIdentifier = null;
                }
                if (remainingTouchCount > 0 && this._longPressTouchIdentifier && !this._activeTouches.has(this._longPressTouchIdentifier)) {
                    if (this._longPressTimer) clearTimeout(this._longPressTimer);
                    this._longPressTimer = null; this._longPressTouchIdentifier = null;
                }
                if (remainingTouchCount === 1) {
                    const [newSingleTouchID] = this._activeTouches.keys();
                    if (this._longPressTouchIdentifier !== newSingleTouchID) {
                        if (this._longPressTimer) clearTimeout(this._longPressTimer);
                        this._longPressTimer = null; this._longPressTouchIdentifier = null;
                        const newTouchData = this._activeTouches.get(newSingleTouchID);
                        if (newTouchData && !newTouchData.longPressCompleted) {
                            const pseudoTouch = { clientX: newTouchData.currentX, clientY: newTouchData.currentY, identifier: newSingleTouchID };
                            this._calculateTouchCoordinates(pseudoTouch);
                            const physicalXAtPressStart = this.x; const physicalYAtPressStart = this.y;
                            this._longPressTouchIdentifier = newSingleTouchID;
                            this._longPressTimer = setTimeout(() => {
                                const currentActiveTouchData = this._activeTouches.get(this._longPressTouchIdentifier);
                                if (currentActiveTouchData && this._activeTouches.size === 1 && this._longPressTouchIdentifier === currentActiveTouchData.identifier && !this._isTwoFingerGesture && this._activeTouchIdentifier === null && !currentActiveTouchData.longPressCompleted) {
                                    const dx = currentActiveTouchData.currentX - currentActiveTouchData.startX;
                                    const dy = currentActiveTouchData.currentY - currentActiveTouchData.startY;
                                    const distSq = dx * dx + dy * dy;
                                    if (distSq < LONG_PRESS_MAX_MOVEMENT_SQ) {
                                        currentActiveTouchData.longPressCompleted = true;
                                        this.x = physicalXAtPressStart; this.y = physicalYAtPressStart;
                                        this.buttonMask |= (1 << 2); this._sendMouseState();
                                        setTimeout(() => { if ((this.buttonMask & (1 << 2)) !== 0) { this.buttonMask &= ~(1 << 2); this._sendMouseState(); } }, 50);
                                    }
                                }
                                this._longPressTimer = null;
                            }, LONG_PRESS_DURATION);
                        }
                    }
                } else if (remainingTouchCount !== 1) {
                     if (this._longPressTimer) clearTimeout(this._longPressTimer);
                     this._longPressTimer = null; this._longPressTouchIdentifier = null;
                }
            }
        }
        if (preventDefault && this.element.contains(event.target)) {
            event.preventDefault();
        }
    }

    _triggerMouseWheel(direction, magnitude) {
        magnitude = Math.max(1, Math.round(magnitude));
        const button = (direction === 'up') ? 4 : 3;
        const mask = 1 << button;

        // In trackpad mode, scroll is a relative event with NO pointer motion.
        // We must always use m2 with a 0,0 delta to prevent the cursor from moving.
        const mtype = "m2";
        const x = 0;
        const y = 0;

        this.buttonMask |= mask;
        this.send([ mtype, x, y, this.buttonMask, magnitude ].join(","));
        setTimeout(() => {
             if ((this.buttonMask & mask) !== 0) {
                this.buttonMask &= ~mask;
                this.send([ mtype, x, y, this.buttonMask, magnitude ].join(","));
             }
        }, 10);
    }

    _triggerHorizontalMouseWheel(direction, magnitude) {
        magnitude = Math.max(1, Math.round(magnitude));
        const button = (direction === 'left') ? 6 : 7;
        const mask = 1 << button;

        // In trackpad mode, scroll is a relative event with NO pointer motion.
        const mtype = "m2";
        const x = 0;
        const y = 0;

        this.buttonMask |= mask;
        this.send([ mtype, x, y, this.buttonMask, magnitude ].join(","));
        setTimeout(() => {
             if ((this.buttonMask & mask) !== 0) {
                this.buttonMask &= ~mask;
                this.send([ mtype, x, y, this.buttonMask, magnitude ].join(","));
             }
        }, 10);
    }

    _dropThreshold() {
        var count = 0;
        var val1 = this._queue.dequeue();
        while (!this._queue.isEmpty()) {
            var valNext = this._queue.dequeue();
            if (valNext >= 80 && val1 == valNext) { count++; }
            val1 = valNext;
        }
        return count >= 2;
    }

    _mouseWheelWrapper(event) {
        var deltaY = Math.trunc(Math.abs(event.deltaY));
        if (this._queue.size() < 4) { this._queue.enqueue(deltaY); }
        if (this._queue.size() == 4) {
            if (this._dropThreshold()) {
                this._allowThreshold = false; this._smallestDeltaY = 10000;
            } else {
                this._allowThreshold = true;
            }
        }
        if (this._allowThreshold && this._allowTrackpadScrolling) {
            this._allowTrackpadScrolling = false;
            this._mouseWheel(event);
            setTimeout(() => this._allowTrackpadScrolling = true, this._wheelThreshold);
        } else if (!this._allowThreshold) {
            this._mouseWheel(event);
        }
        event.preventDefault();
    }

    _mouseWheel(event) {
        if (event.deltaY !== 0) {
            const direction = (event.deltaY < 0) ? 'up' : 'down';
            let deltaY = Math.abs(Math.trunc(event.deltaY));
            if (deltaY < this._smallestDeltaY && deltaY !== 0) {
                this._smallestDeltaY = deltaY;
            }
            const verticalMagnitude = Math.max(1, Math.floor(deltaY / this._smallestDeltaY));
            const magnitude = Math.min(verticalMagnitude, this._scrollMagnitude);
            this._triggerMouseWheel(direction, magnitude);
        }
        if (event.deltaX !== 0) {
            const direction = (event.deltaX < 0) ? 'left' : 'right';
            const horizontalMagnitude = Math.max(1, Math.round(Math.abs(event.deltaX) / 100));
            const magnitude = Math.min(horizontalMagnitude, this._scrollMagnitude);
            this._triggerHorizontalMouseWheel(direction, magnitude);
        }
    }

    _contextMenu(event) {
        if (this.element.contains(event.target)) {
            event.preventDefault();
        }
    }

    _pointerLock() {
        if (document.pointerLockElement === this.element) {
            this.send("p,1");
            this.send("SET_NATIVE_CURSOR_RENDERING,1");
        } else {
            this.send("p,0");
            this.send("SET_NATIVE_CURSOR_RENDERING,0");
            this.resetKeyboard();
        }
    }

    _windowMath() {
        const elementRect = this.element.getBoundingClientRect();
        const windowW = elementRect.width; const windowH = elementRect.height;
        const frameW = this.element.offsetWidth; const frameH = this.element.offsetHeight;
        if (windowW <= 0 || windowH <= 0 || frameW <= 0 || frameH <= 0) { this.m = null; return; }
        const multiX = windowW / frameW; const multiY = windowH / frameH;
        const multi = Math.min(multiX, multiY);
        const vpWidth = frameW * multi; const vpHeight = frameH * multi;
        const offsetX = (windowW - vpWidth) / 2.0; const offsetY = (windowH - vpHeight) / 2.0;
        const mouseMultiX = (vpWidth > 0) ? frameW / vpWidth : 1;
        const mouseMultiY = (vpHeight > 0) ? frameH / vpHeight : 1;
        this.m = {
            mouseMultiX, mouseMultiY, mouseOffsetX: offsetX, mouseOffsetY: offsetY,
            elementClientX: elementRect.left, elementClientY: elementRect.top,
            frameW, frameH,
        };
    }

    _clientToServerX(clientX) {
        if (!this.m) return 0;
        const elementRelativeX = clientX - this.m.elementClientX;
        const viewportRelativeX = elementRelativeX - this.m.mouseOffsetX;
        let serverX = viewportRelativeX * this.m.mouseMultiX;
        return Math.round(serverX);
    }

    _clientToServerY(clientY) {
        if (!this.m) return 0;
        const elementRelativeY = clientY - this.m.elementClientY;
        const viewportRelativeY = elementRelativeY - this.m.mouseOffsetY;
        let serverY = viewportRelativeY * this.m.mouseMultiY;
        return Math.round(serverY);
    }

    _gamepadConnected(event) {
        const server_gp_index = (this.controllerSlot !== null) ? this.controllerSlot - 1 : this.playerIndex;
        if (server_gp_index === undefined || server_gp_index === null) return;
        if (!this.gamepadManager) {
            this.gamepadManager = new GamepadManager(event.gamepad, this._gamepadButton.bind(this), this._gamepadAxis.bind(this));
        }
        const connectMsg = "js,c," + server_gp_index + "," + btoa(event.gamepad.id) + "," + event.gamepad.axes.length + "," + event.gamepad.buttons.length;
        this.send(connectMsg);
        if (this.ongamepadconnected !== null) { this.ongamepadconnected(event.gamepad.id); }
    }

    _gamepadDisconnect(event) {
         if (this.ongamepaddisconneceted !== null) { this.ongamepaddisconneceted(); }
         const server_gp_index = (this.controllerSlot !== null) ? this.controllerSlot - 1 : this.playerIndex;
         if (server_gp_index === undefined || server_gp_index === null) return;
         this.send("js,d," + server_gp_index);
    }

    _gamepadButton(gp_num, btn_num, val) {
        const server_gp_index = (this.controllerSlot !== null) ? this.controllerSlot - 1 : this.playerIndex;
        if (server_gp_index === undefined || server_gp_index < 0) return;

        this.send("js,b," + server_gp_index + "," + btn_num + "," + val);
        if (this._isSidebarOpen) {
            window.postMessage({ type: 'gamepadButtonUpdate', gamepadIndex: server_gp_index, buttonIndex: btn_num, value: val }, window.location.origin);
        }
    }

    _gamepadAxis(gp_num, axis_num, val) {
        const server_gp_index = (this.controllerSlot !== null) ? this.controllerSlot - 1 : this.playerIndex;
        if (server_gp_index === undefined || server_gp_index < 0) return;

        this.send("js,a," + server_gp_index + "," + axis_num + "," + val);
        if (this._isSidebarOpen) {
            window.postMessage({ type: 'gamepadAxisUpdate', gamepadIndex: server_gp_index, axisIndex: axis_num, value: val }, window.location.origin);
        }
    }

    _onFullscreenChange() {
        if (document.fullscreenElement === this.element.parentElement) {
            if (document.pointerLockElement !== this.element) {
                this.element.requestPointerLock().catch(err => console.warn("Pointer lock failed on fullscreen:", err));
            }
            this.requestKeyboardLock();
        } else {
            if (document.pointerLockElement === this.element) {
                 document.exitPointerLock();
            }
            this.send("kr");
            this.resetKeyboard();
        }
    }

    _targetHasClass(target, className) {
        let element = target;
        while (element && element.classList) {
            if (element.classList.contains(className)) return true;
            element = element.parentElement;
        }
        return false;
    }

    getWindowResolution() {
        const bodyWidth = document.body ? document.body.offsetWidth : window.innerWidth;
        const bodyHeight = document.body ? document.body.offsetHeight : window.innerHeight;
        const ratio = window.devicePixelRatio || 1;
        const offsetRatioWidth = bodyWidth * ratio;
        const offsetRatioHeight = bodyHeight * ratio;
        return [ Math.max(1, parseInt(offsetRatioWidth - offsetRatioWidth % 2)), Math.max(1, parseInt(offsetRatioHeight - offsetRatioHeight % 2)) ];
    }

    isInputAttached() {
        return this.inputAttached;
    }

    attach() {
        this.listeners.push(addListener(this.element, 'resize', this._windowMath, this));
        this.listeners.push(addListener(document, 'pointerlockchange', this._pointerLock, this));
        this.listeners.push(addListener(document, 'fullscreenchange', this._onFullscreenChange, this));
        this.listeners.push(addListener(window, 'resize', this._windowMath, this));
        this.listeners.push(addListener(window, 'gamepadconnected', this._gamepadConnected, this));
        this.listeners.push(addListener(window, 'gamepaddisconnected', this._gamepadDisconnect, this));
        this.listeners.push(addListener(window, 'message', this._handleVisibilityMessage, this));

        if (!this.isSharedMode) {
            this.attach_context();
        } else {
            const preventDefaultHandler = (e) => e.preventDefault();
            this.listeners.push(addListener(this.element, 'touchstart', preventDefaultHandler, this));
            this.listeners.push(addListener(this.element, 'touchend', preventDefaultHandler, this));
            this.listeners.push(addListener(this.element, 'touchmove', preventDefaultHandler, this));
            this.listeners.push(addListener(this.element, 'touchcancel', preventDefaultHandler, this));
        }    
    }

    attach_context() {
        if (this.inputAttached) return;
        this._windowMath();
        this.element.style.setProperty('cursor', 'none', 'important');
        if (this._cursorImageBitmap || this._cursorBase64Data) {
            if (this.use_browser_cursors) {
                this._updateBrowserCursor();
            } else {
                this.cursorDiv.style.display = 'block';
                this._drawAndScaleCursor();
            }
        }
        this.listeners_context.push(addListener(window, 'keydown', this._handleKeyDown, this, true));
        this.listeners_context.push(addListener(window, 'keyup', this._handleKeyUp, this, true));
        this.listeners_context.push(addListener(window, 'blur', this.resetKeyboard, this));
        this.listeners_context.push(addListener(this.keyboardInputAssist, 'input', this._handleMobileInput, this));
        this.listeners_context.push(addListener(document, 'mousedown', this._handleOutsideClick, this, true));
        this.listeners_context.push(addListener(document, 'touchstart', this._handleOutsideClick, this, true));

        this.listeners_context.push(addListener(this.element, 'wheel', this._mouseWheelWrapper, this));
        this.listeners_context.push(addListener(this.element, 'contextmenu', this._contextMenu, this));

        const compositionTarget = this.element;
        this.listeners_context.push(addListener(compositionTarget, 'compositionstart', this._compositionStart, this));
        this.listeners_context.push(addListener(compositionTarget, 'compositionupdate', this._compositionUpdate, this));
        this.listeners_context.push(addListener(compositionTarget, 'compositionend', this._compositionEnd, this));
        if (browser.isLinux()) {
            this.listeners_context.push(addListener(this.element, 'textInput', this._handleTextInput, this));
        }
        this.listeners_context.push(addListener(this.element, 'pointerdown', this._handlePointerDown, this));
        this.listeners_context.push(addListener(this.element, 'pointermove', this._handlePointerMove, this));
        this.listeners_context.push(addListener(this.element, 'pointerup', this._handlePointerUp, this));
        this.listeners_context.push(addListener(this.element, 'pointercancel', this._handlePointerUp, this)); 

        if ('ontouchstart' in window) {
            this.listeners_context.push(addListener(this.element, 'touchstart', this._handleTouchEvent, this, false));
            this.listeners_context.push(addListener(this.element, 'touchend', this._handleTouchEvent, this, false));
            this.listeners_context.push(addListener(this.element, 'touchmove', this._handleTouchEvent, this, false));
            this.listeners_context.push(addListener(this.element, 'touchcancel', this._handleTouchEvent, this, false));
        }
        this.listeners_context.push(addListener(this.element, 'mousedown', this._mouseButtonMovement, this));
        this.listeners_context.push(addListener(window, 'mousemove', this._mouseButtonMovement, this));
        this.listeners_context.push(addListener(window, 'mouseup', this._mouseButtonMovement, this));

        if (document.fullscreenElement === this.element.parentElement) {
             if (document.pointerLockElement !== this.element) {
                this.element.requestPointerLock().catch(()=>{});
             }
             this.requestKeyboardLock();
        } else if (document.pointerLockElement === this.element) {
             this._pointerLock();
        }
        this._windowMath();
        this.inputAttached = true;
    }

    detach() {
        removeListeners(this.listeners);
        this.listeners = [];
        if (this.gamepadManager) {
            this.gamepadManager.destroy();
            this.gamepadManager = null;
        }
        this.detach_context();
    }

    detach_context() {
        removeListeners(this.listeners_context);
        this.listeners_context = [];
        this.element.style.cursor = 'auto';
        this.cursorDiv.style.display = 'none';
        this.send("kr");
        this.resetKeyboard();
        this._activeTouches.clear();
        this._activeTouchIdentifier = null;
        this._isTwoFingerGesture = false;
        if ((this.buttonMask & 1) === 1) {
             this.buttonMask &= ~1;
             this._sendMouseState();
        }
        this.inputAttached = false;
        this._exitPointerLock();
    }

    _exitPointerLock() {
        if (document.pointerLockElement === this.element) {
            document.exitPointerLock();
        }
    }

    enterFullscreen() {
        if (this.element.parentElement && document.fullscreenElement === null) {
            this.element.parentElement.requestFullscreen()
                .catch(err => console.error("Fullscreen request failed:", err));
        } else if (document.fullscreenElement !== null && document.pointerLockElement !== this.element) {
             this.element.requestPointerLock().catch(()=>{});
        }
    }

    requestKeyboardLock() {
        if (document.fullscreenElement && 'keyboard' in navigator && 'lock' in navigator.keyboard) {
            const keys = [ "AltLeft", "AltRight", "Tab", "Escape", "MetaLeft", "MetaRight", "ContextMenu" ];
            navigator.keyboard.lock(keys).then(() => {
                // console.log('Keyboard lock active.');
            }).catch(err => {
                // console.warn('Keyboard lock failed:', err);
            });
        }
    }
}

function addListener(obj, name, func, ctx, useCapture = false) {
    if (!obj || typeof obj.addEventListener !== 'function') {
        console.error("addListener: Invalid target object", obj);
        return null;
    }
    const newFunc = ctx ? func.bind(ctx) : func;
    const options = { capture: useCapture, passive: false }; // Set passive: false for preventDefault
    obj.addEventListener(name, newFunc, options);
    return [obj, name, newFunc, options];
}

function removeListeners(listeners) {
    for (const listener of listeners) {
        if (listener && listener[0] && typeof listener[0].removeEventListener === 'function') {
            listener[0].removeEventListener(listener[1], listener[2], listener[3]);
        }
    }
    listeners.length = 0;
}
