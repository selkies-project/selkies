// src/components/Sidebar.jsx
import React, { useState, useEffect, useCallback, useRef } from "react";
import GamepadVisualizer from "./GamepadVisualizer";
import { getTranslator } from "../translations";
import yaml from "js-yaml";

// --- Constants ---
const urlHash = window.location.hash;
const displayId = urlHash.startsWith('#display2') ? 'display2' : 'primary';

const PER_DISPLAY_SETTINGS = [
    'framerate', 'h264_crf', 'h264_fullcolor',
    'h264_streaming_mode', 'jpeg_quality', 'paint_over_jpeg_quality', 'use_cpu',
    'h264_paintover_crf', 'h264_paintover_burst_frames', 'use_paint_over_quality',
    'is_manual_resolution_mode', 'manual_width', 'manual_height',
    'encoder', 'scaleLocallyManual', 'use_browser_cursors'
];

const encoderOptions = [
  "x264enc",
  "x264enc-striped",
  "jpeg",
];

const commonResolutionValues = [
  "",
  "1920x1080",
  "1280x720",
  "1366x768",
  "1920x1200",
  "2560x1440",
  "3840x2160",
  "1024x768",
  "800x600",
  "640x480",
  "320x240",
];

const dpiScalingOptions = [
  { label: "100%", value: 96 },
  { label: "125%", value: 120 },
  { label: "150%", value: 144 },
  { label: "175%", value: 168 },
  { label: "200%", value: 192 },
  { label: "225%", value: 216 },
  { label: "250%", value: 240 },
  { label: "275%", value: 264 },
  { label: "300%", value: 288 },
];
const DEFAULT_SCALING_DPI = 96;

const STATS_READ_INTERVAL_MS = 500;
const MAX_AUDIO_BUFFER = 10;
const DEFAULT_FRAMERATE = 60;
const DEFAULT_JPEG_QUALITY = 60;
const DEFAULT_PAINT_OVER_JPEG_QUALITY = 90;
const DEFAULT_USE_CPU = false;
const DEFAULT_H264_PAINTOVER_CRF = 18;
const DEFAULT_USE_PAINT_OVER_QUALITY = true;
const DEFAULT_VIDEO_BUFFER_SIZE = 0;
const DEFAULT_ENCODER = encoderOptions[0];
const DEFAULT_VIDEO_CRF = 25;
const DEFAULT_SCALE_LOCALLY = true;
const DEFAULT_ENABLE_BINARY_CLIPBOARD = false;
const REPO_BASE_URL =
  "https://raw.githubusercontent.com/linuxserver/proot-apps/master/metadata/";
const METADATA_URL = `${REPO_BASE_URL}metadata.yml`;
const IMAGE_BASE_URL = `${REPO_BASE_URL}img/`;

const MAX_NOTIFICATIONS = 3;
const NOTIFICATION_TIMEOUT_SUCCESS = 5000;
const NOTIFICATION_TIMEOUT_ERROR = 8000;
const NOTIFICATION_FADE_DURATION = 500;

const TOUCH_GAMEPAD_HOST_DIV_ID = "touch-gamepad-host";

const STREAM_MODE_WEBRTC = "webrtc";
const STREAM_MODE_WEBSOCKETS = "websockets";
const STREAMING_MODES= [STREAM_MODE_WEBRTC, STREAM_MODE_WEBSOCKETS]
const DEFAULT_STREAM_MODE = STREAM_MODE_WEBSOCKETS;
const DEFAULT_WEBRTC_ENCODER = "x264enc";
const DEFAULT_AUDIO_BITRATE = 96000;  // in bps
const DEFAULT_VIDEO_BITRATE = 8000;   // in kbps

// --- Helper Functions ---
function formatBytes(bytes, decimals = 2, rawDict) {
  const zeroBytesText = rawDict?.zeroBytes || "0 Bytes";
  if (bytes === null || bytes === undefined || bytes === 0)
    return zeroBytesText;
  const k = 1024;
  const dm = decimals < 0 ? 0 : decimals;
  const sizes = rawDict?.byteUnits || [
    "Bytes",
    "KB",
    "MB",
    "GB",
    "TB",
    "PB",
    "EB",
    "ZB",
    "YB",
  ];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  const unitIndex = Math.min(i, sizes.length - 1);
  return (
    parseFloat((bytes / Math.pow(k, i)).toFixed(dm)) + " " + sizes[unitIndex]
  );
}

const calculateGaugeOffset = (percentage, radius, circumference) => {
  const clampedPercentage = Math.max(0, Math.min(100, percentage || 0));
  return circumference * (1 - clampedPercentage / 100);
};

const roundDownToEven = (num) => {
  const n = parseInt(num, 10);
  if (isNaN(n)) return 0;
  return Math.floor(n / 2) * 2;
};

// Debounce function
function debounce(func, delay) {
  let timeoutId;
  return function (...args) {
    const context = this;
    clearTimeout(timeoutId);
    timeoutId = setTimeout(() => {
      func.apply(context, args);
    }, delay);
  };
}

// --- Icons ---
const CopyIcon = () => (
  <svg viewBox="0 0 24 24" fill="currentColor" width="16" height="16" style={{ display: 'block' }}>
    <path d="M16 1H4c-1.1 0-2 .9-2 2v14h2V3h12V1zm3 4H8c-1.1 0-2 .9-2 2v14c0 1.1.9 2 2 2h11c1.1 0 2-.9 2-2V7c0-1.1-.9-2-2-2zm0 16H8V7h11v14z"/>
  </svg>
);
const GamingModeIcon = () => (
  <svg viewBox="0 0 24 24" stroke="currentColor" strokeWidth="2" fill="none" width="18" height="18">
    <circle cx="12" cy="12" r="1.5" fill="currentColor" />
    <path d="M12 5V9M12 15V19M5 12H9M15 12H19" strokeLinecap="round" />
  </svg>
);
const AppsIcon = () => (
  <svg viewBox="0 0 24 24" fill="currentColor" width="20" height="20">
    <path d="M4 8h4V4H4v4zm6 12h4v-4h-4v4zm-6 0h4v-4H4v4zm0-6h4v-4H4v4zm6 0h4v-4h-4v4zm6-10v4h4V4h-4zm-6 4h4V4h-4v4zm6 6h4v-4h-4v4zm0 6h4v-4h-4v4z" />
  </svg>
);
const KeyboardIcon = () => (
  <svg 
    xmlns="http://www.w3.org/2000/svg" 
    viewBox="0 0 490 490" 
    fill="currentColor" 
    width="24" 
    height="24"
  >
    <path d="M251.2 193.5v-53.7a10.5 10.5 0 0 1 10.5-10.5h119.4c21 0 38.1-17.1 38.1-38.1s-17.1-38.1-38.1-38.1H129.5c-5.4 0-10.1 4.3-10.1 10.1s4.3 10.1 10.1 10.1h251.6c10.1 0 17.9 8.2 17.9 17.9 0 10.1-8.2 17.9-17.9 17.9H261.7c-16.7 0-30.3 13.6-30.3 30.3v53.3H0v244.2h490V193.5H251.2zm-19 28h15.6c5.4 0 10.1 4.3 10.1 10.1s-4.3 10.1-10.1 10.1h-15.6c-5.4 0-10.1-4.3-10.1-10.1s4.6-10.1 10.1-10.1zm-28.8 104.2h-15.6c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1 0 5.4-4.7 10.1-10.1 10.1zm10.1 27.2c0 5.4-4.3 10.1-10.1 10.1h-15.6c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.7 10.1 10.1zM203.4 288h-15.6c-5.4 0-10.1-4.3-10.1-10.1s4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1s-4.7 10.1-10.1 10.1zm-17.1-66.5h15.6c5.4 0 10.1 4.3 10.1 10.1s-4.3 10.1-10.1 10.1h-15.6c-5.4 0-10.1-4.3-10.1-10.1s4.6-10.1 10.1-10.1zm-45.9 0H156c5.4 0 10.1 4.3 10.1 10.1s-4.3 10.1-10.1 10.1h-15.6c-5.4 0-10.1-4.3-10.1-10.1s4.6-10.1 10.1-10.1zm-1.6 46.6h15.6c5.4 0 10.1 4.3 10.1 10.1s-4.3 10.1-10.1 10.1h-15.6c-5.4 0-10.1-4.3-10.1-10.1s4.7-10.1 10.1-10.1zm0 37.4h15.6c5.4 0 10.1 4.3 10.1 10.1 0 5.4-4.3 10.1-10.1 10.1h-15.6c-5.4 0-10.1-4.3-10.1-10.1 0-5.5 4.7-10.1 10.1-10.1zm0 37.3h15.6c5.4 0 10.1 4.3 10.1 10.1 0 5.4-4.3 10.1-10.1 10.1h-15.6c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.7-10.1 10.1-10.1zM94.5 221.5h15.6c5.4 0 10.1 4.3 10.1 10.1s-4.3 10.1-10.1 10.1H94.5c-5.4 0-10.1-4.3-10.1-10.1s4.7-10.1 10.1-10.1zm-5.1 46.6H105c5.4 0 10.1 4.3 10.1 10.1s-4.3 10.1-10.1 10.1H89.4c-5.4 0-10.1-4.3-10.1-10.1s4.7-10.1 10.1-10.1zm0 37.4H105c5.4 0 10.1 4.3 10.1 10.1 0 5.4-4.3 10.1-10.1 10.1H89.4c-5.4 0-10.1-4.3-10.1-10.1.4-5.5 4.7-10.1 10.1-10.1zm0 37.3H105c5.4 0 10.1 4.3 10.1 10.1 0 5.4-4.3 10.1-10.1 10.1H89.4c-5.4 0-10.1-4.3-10.1-10.1.4-5.4 4.7-10.1 10.1-10.1zM56 400.4H40.4c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1H56c5.4 0 10.1 4.3 10.1 10.1-.4 5.4-4.7 10.1-10.1 10.1zm0-37.4H40.4c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1H56c5.4 0 10.1 4.3 10.1 10.1-.4 5.5-4.7 10.1-10.1 10.1zm0-37.3H40.4c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1H56c5.4 0 10.1 4.3 10.1 10.1-.4 5.4-4.7 10.1-10.1 10.1zm0-37.7H40.4c-5.4 0-10.1-4.3-10.1-10.1s4.3-10.1 10.1-10.1H56c5.4 0 10.1 4.3 10.1 10.1S61.4 288 56 288zm0-46.7H40.4c-5.4 0-10.1-4.3-10.1-10.1s4.3-10.1 10.1-10.1H56c5.4 0 10.1 4.3 10.1 10.1s-4.7 10.1-10.1 10.1zm196.8 159.1H89.4c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h163.3c5.4 0 10.1 4.3 10.1 10.1.1 5.4-4.6 10.1-10 10.1zm0-37.4h-15.6c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1 0 5.5-4.7 10.1-10.1 10.1zm0-37.3h-15.6c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1 0 5.4-4.7 10.1-10.1 10.1zm0-37.7h-15.6c-5.4 0-10.1-4.3-10.1-10.1s4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1s-4.7 10.1-10.1 10.1zm49.4 112.4h-15.6c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1-.4 5.4-4.7 10.1-10.1 10.1zm0-37.4h-15.6c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1-.4 5.5-4.7 10.1-10.1 10.1zm0-37.3h-15.6c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1-.4 5.4-4.7 10.1-10.1 10.1zm0-37.7h-15.6c-5.4 0-10.1-4.3-10.1-10.1s4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1s-4.7 10.1-10.1 10.1zm10.1-46.7h-15.6c-5.4 0-10.1-4.3-10.1-10.1s4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1s-4.7 10.1-10.1 10.1zm38.9 159.1h-15.6c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1 0 5.4-4.7 10.1-10.1 10.1zm0-37.4h-15.6c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1 0 5.5-4.7 10.1-10.1 10.1zm0-37.3h-15.6c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1 0 5.4-4.7 10.1-10.1 10.1zm0-37.7h-15.6c-5.4 0-10.1-4.3-10.1-10.1s4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1s-4.7 10.1-10.1 10.1zm6.6-46.7h-15.6c-5.4 0-10.1-4.3-10.1-10.1s4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1s-4.3 10.1-10.1 10.1zm42.8 159.1H385c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1-.4 5.4-4.7 10.1-10.1 10.1zm0-37.4H385c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1-.4 5.5-4.7 10.1-10.1 10.1zm0-37.3H385c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1-.4 5.4-4.7 10.1-10.1 10.1zm0-37.7H385c-5.4 0-10.1-4.3-10.1-10.1s4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1S406 288 400.6 288zm3.1-46.7h-15.6c-5.4 0-10.1-4.3-10.1-10.1s4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1s-4.3 10.1-10.1 10.1zm45.9 159.1H434c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1 0 5.4-4.7 10.1-10.1 10.1zm0-37.4H434c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1 0 5.5-4.7 10.1-10.1 10.1zm0-37.3H434c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1 0 5.4-4.7 10.1-10.1 10.1zm0-37.7H434c-5.4 0-10.1-4.3-10.1-10.1s4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1S455 288 449.6 288zm0-46.7H434c-5.4 0-10.1-4.3-10.1-10.1s4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1s-4.7 10.1-10.1 10.1z" />
  </svg>
);
const ScreenIcon = () => (
  <svg viewBox="0 0 24 24" fill="currentColor" width="20" height="20">
    <path d="M20 18c1.1 0 1.99-.9 1.99-2L22 6c0-1.1-.9-2-2-2H4c-1.1 0-2 .9-2 2v10c0 1.1.9 2 2 2H0v2h24v-2h-4zM4 6h16v10H4V6z" />
  </svg>
);
const SpeakerIcon = () => (
  <svg viewBox="0 0 24 24" fill="currentColor" width="20" height="20">
    <path d="M3 9v6h4l5 5V4L7 9H3zm13.5 3c0-1.77-1.02-3.29-2.5-4.03v8.05c1.48-.73 2.5-2.25 2.5-4.02zM14 3.23v2.06c2.89.86 5 3.54 5 6.71s-2.11 5.85-5 6.71v2.06c4.01-.91 7-4.49 7-8.77s-2.99-7.86-7-8.77z" />
  </svg>
);
const MicrophoneIcon = () => (
  <svg viewBox="0 0 24 24" fill="currentColor" width="20" height="20">
    <path d="M12 14c1.66 0 2.99-1.34 2.99-3L15 5c0-1.66-1.34-3-3-3S9 3.34 9 5v6c0 1.66 1.34 3 3 3zm5.3-3c0 3-2.54 5.1-5.3 5.1S6.7 14 6.7 11H5c0 3.41 2.72 6.23 6 6.72V21h2v-3.28c3.28-.48 6-3.3 6-6.72h-1.7z" />
  </svg>
);
const GamepadIcon = () => (
  <svg viewBox="0 0 24 24" fill="currentColor" width="20" height="20">
    <path d="M15 7.5V2H9v5.5l3 3 3-3zM7.5 9H2v6h5.5l3-3-3-3zM9 16.5V22h6v-5.5l-3-3-3 3zM16.5 9l-3 3 3 3H22V9h-5.5z" />
  </svg>
);
const TrackpadIcon = () => (
  <svg viewBox="0 0 24 24" fill="currentColor" width="18" height="18">
    <path d="M3 5C3 3.89543 3.89543 3 5 3H19C20.1046 3 21 3.89543 21 5V15H3V5Z"/>
    <path d="M3 16H11V21H5C3.89543 21 3 20.1046 3 19V16Z"/>
    <path d="M13 16H21V19C21 20.1046 20.1046 21 19 21H13V16Z"/>
  </svg>
);
const FullscreenIcon = () => (
  <svg viewBox="0 0 24 24" fill="currentColor" width="18" height="18">
    <path d="M7 14H5v5h5v-2H7v-3zm-2-4h2V7h3V5H5v5zm12 7h-3v2h5v-5h-2v3zM14 5v2h3v3h2V5h-5z" />
  </svg>
);
const CaretDownIcon = () => (
  <svg
    viewBox="0 0 24 24"
    fill="currentColor"
    width="18"
    height="18"
    style={{ display: "block" }}
  >
    <path d="M7 10l5 5 5-5H7z" />
  </svg>
);
const CaretUpIcon = () => (
  <svg
    viewBox="0 0 24 24"
    fill="currentColor"
    width="18"
    height="18"
    style={{ display: "block" }}
  >
    <path d="M7 14l5-5 5 5H7z" />
  </svg>
);
const SpinnerIcon = () => (
  <svg
    width="18"
    height="18"
    viewBox="0 0 38 38"
    xmlns="http://www.w3.org/2000/svg"
    stroke="currentColor"
  >
    <g fill="none" fillRule="evenodd">
      <g transform="translate(1 1)" strokeWidth="3">
        <circle strokeOpacity=".3" cx="18" cy="18" r="18" />
        <path d="M36 18c0-9.94-8.06-18-18-18">
          <animateTransform
            attributeName="transform"
            type="rotate"
            from="0 18 18"
            to="360 18 18"
            dur="0.8s"
            repeatCount="indefinite"
          />
        </path>
      </g>
    </g>
  </svg>
);
// --- End Icons ---

const SelkiesLogo = ({ width = 30, height = 30, className, t, ...props }) => (
  <svg
    xmlns="http://www.w3.org/2000/svg"
    viewBox="0 0 200 200"
    width={width}
    height={height}
    className={className}
    role="img"
    aria-label={t("selkiesLogoAlt")}
    {...props}
  >
    <path
      fill="#61dafb"
      d="M156.825 120.999H5.273l-.271-1.13 87.336-43.332-7.278 17.696c4 1.628 6.179.541 7.907-2.974l26.873-53.575c1.198-2.319 3.879-4.593 6.358-5.401 9.959-3.249 20.065-6.091 30.229-8.634 1.9-.475 4.981.461 6.368 1.873 4.067 4.142 7.32 9.082 11.379 13.233 1.719 1.758 4.572 2.964 7.058 3.29 4.094.536 8.311.046 12.471.183 5.2.171 6.765 2.967 4.229 7.607-2.154 3.942-4.258 7.97-6.94 11.542-1.264 1.684-3.789 3.274-5.82 3.377-7.701.391-15.434.158-23.409 1.265 2.214 1.33 4.301 2.981 6.67 3.919 4.287 1.698 5.76 4.897 6.346 9.162 1.063 7.741 2.609 15.417 3.623 23.164.22 1.677-.464 3.971-1.579 5.233-3.521 3.987-7.156 7.989-11.332 11.232-2.069 1.607-5.418 1.565-8.664 2.27m-3.804-69.578c5.601.881 6.567-5.024 11.089-6.722l-9.884-7.716-11.299 9.983 10.094 4.455z"
    />
    <path
      fill="#61dafb"
      d="M86 131.92c7.491 0 14.495.261 21.467-.1 4.011-.208 6.165 1.249 7.532 4.832 1.103 2.889 2.605 5.626 4.397 9.419h-93.41l5.163 24.027-1.01.859c-3.291-2.273-6.357-5.009-9.914-6.733-11.515-5.581-17.057-14.489-16.403-27.286.073-1.423-.287-2.869-.525-5.019H86z"
    />
    <path
      fill="#61dafb"
      d="M129.004 164.999l1.179-1.424c9.132-10.114 9.127-10.11 2.877-22.425l-4.552-9.232c4.752 0 8.69.546 12.42-.101 11.96-2.075 20.504 1.972 25.74 13.014.826 1.743 2.245 3.205 3.797 5.361-9.923 7.274-19.044 15.174-29.357 20.945-4.365 2.443-11.236.407-17.714.407l5.611-6.545z"
    />
    <path
      fill="#FFFFFF"
      d="M152.672 51.269l-9.745-4.303 11.299-9.983 9.884 7.716c-4.522 1.698-5.488 7.602-11.439 6.57z"
    />
  </svg>
);

const INSTALLED_APPS_STORAGE_KEY = "prootInstalledApps";
function AppsModal({ isOpen, onClose, t }) {
  const [appData, setAppData] = useState(null);
  const [isLoading, setIsLoading] = useState(false);
  const [error, setError] = useState(null);
  const [searchTerm, setSearchTerm] = useState("");
  const [selectedApp, setSelectedApp] = useState(null);
  const [installedApps, setInstalledApps] = useState(() => {
    const savedApps = localStorage.getItem(INSTALLED_APPS_STORAGE_KEY);
    if (savedApps) {
      try {
        const parsedApps = JSON.parse(savedApps);
        if (
          Array.isArray(parsedApps) &&
          parsedApps.every((item) => typeof item === "string")
        ) {
          return parsedApps;
        }
        console.warn(
          "Invalid data found in localStorage for installed apps. Resetting."
        );
        localStorage.removeItem(INSTALLED_APPS_STORAGE_KEY);
      } catch (e) {
        console.error("Failed to parse installed apps from localStorage:", e);
        localStorage.removeItem(INSTALLED_APPS_STORAGE_KEY);
      }
    }
    return [];
  });

  useEffect(() => {
    localStorage.setItem(
      INSTALLED_APPS_STORAGE_KEY,
      JSON.stringify(installedApps)
    );
  }, [installedApps]);

  useEffect(() => {
    if (isOpen && !appData && !isLoading) {
      const fetchAppData = async () => {
        setIsLoading(true);
        setError(null);
        try {
          const response = await fetch(METADATA_URL);
          if (!response.ok) {
            throw new Error(`HTTP error! status: ${response.status}`);
          }
          const yamlText = await response.text();
          const parsedData = yaml.load(yamlText);
          setAppData(parsedData);
        } catch (e) {
          console.error("Failed to fetch or parse app data:", e);
          setError(
            t(
              "appsModal.errorLoading",
              "Failed to load app data. Please try again."
            )
          );
        } finally {
          setIsLoading(false);
        }
      };
      fetchAppData();
    }
  }, [isOpen, appData, isLoading, t, yaml]);

  const handleSearchChange = (event) =>
    setSearchTerm(event.target.value.toLowerCase());
  const handleAppClick = (app) => setSelectedApp(app);
  const handleBackToGrid = () => setSelectedApp(null);

  const handleInstall = (appName) => {
    console.log(`Install app: ${appName}`);
    window.postMessage(
      {
        type: "command",
        value: `st ~/.local/bin/proot-apps install ${appName}`,
      },
      window.location.origin
    );
    setInstalledApps((prev) =>
      prev.includes(appName) ? prev : [...prev, appName]
    );
  };
  const handleRemove = (appName) => {
    console.log(`Remove app: ${appName}`);
    window.postMessage(
      {
        type: "command",
        value: `st ~/.local/bin/proot-apps remove ${appName}`,
      },
      window.location.origin
    );
    setInstalledApps((prev) => prev.filter((name) => name !== appName));
  };
  const handleUpdate = (appName) => {
    console.log(`Update app: ${appName}`);
    window.postMessage(
      {
        type: "command",
        value: `st ~/.local/bin/proot-apps update ${appName}`,
      },
      window.location.origin
    );
  };

  const filteredApps =
    appData?.include?.filter(
      (app) =>
        !app.disabled &&
        (app.full_name?.toLowerCase().includes(searchTerm) ||
          app.name?.toLowerCase().includes(searchTerm) ||
          app.description?.toLowerCase().includes(searchTerm))
    ) || [];
  const isAppInstalled = (appName) => installedApps.includes(appName);

  if (!isOpen) return null;

  return (
    <div className="apps-modal">
      <button
        className="apps-modal-close"
        onClick={onClose}
        aria-label={t("appsModal.closeAlt", "Close apps modal")}
      >
        &times;
      </button>
      <div className="apps-modal-content">
        {isLoading && (
          <div className="apps-modal-loading">
            <SpinnerIcon />
            <p>{t("appsModal.loading", "Loading apps...")}</p>
          </div>
        )}
        {error && <p className="apps-modal-error">{error}</p>}
        {!isLoading && !error && appData && (
          <>
            {selectedApp ? (
              <div className="app-detail-view">
                <button
                  onClick={handleBackToGrid}
                  className="app-detail-back-button"
                >
                  &larr; {t("appsModal.backButton", "Back to list")}
                </button>
                <img
                  src={`${IMAGE_BASE_URL}${selectedApp.icon}`}
                  alt={selectedApp.full_name}
                  className="app-detail-icon"
                  onError={(e) => {
                    e.target.style.display = "none";
                  }}
                />
                <h2>{selectedApp.full_name}</h2>
                <p className="app-detail-description">
                  {selectedApp.description}
                </p>
                <div className="app-action-buttons">
                  {isAppInstalled(selectedApp.name) ? (
                    <>
                      <button
                        onClick={() => handleUpdate(selectedApp.name)}
                        className="app-action-button update"
                      >
                        {t("appsModal.updateButton", "Update")}{" "}
                        {selectedApp.name}
                      </button>
                      <button
                        onClick={() => handleRemove(selectedApp.name)}
                        className="app-action-button remove"
                      >
                        {t("appsModal.removeButton", "Remove")}{" "}
                        {selectedApp.name}
                      </button>
                    </>
                  ) : (
                    <button
                      onClick={() => handleInstall(selectedApp.name)}
                      className="app-action-button install"
                    >
                      {t("appsModal.installButton", "Install")}{" "}
                      {selectedApp.name}
                    </button>
                  )}
                </div>
              </div>
            ) : (
              <>
                <input
                  type="text"
                  className="apps-search-bar allow-native-input"
                  placeholder={t(
                    "appsModal.searchPlaceholder",
                    "Search apps..."
                  )}
                  value={searchTerm}
                  onChange={handleSearchChange}
                />
                <div className="apps-grid">
                  {filteredApps.length > 0 ? (
                    filteredApps.map((app) => (
                      <div
                        key={app.name}
                        className="app-card"
                        onClick={() => handleAppClick(app)}
                      >
                        <img
                          src={`${IMAGE_BASE_URL}${app.icon}`}
                          alt={app.full_name}
                          className="app-card-icon"
                          loading="lazy"
                          onError={(e) => {
                            e.target.style.visibility = "hidden";
                          }}
                        />
                        <p className="app-card-name">{app.full_name}</p>
                        {isAppInstalled(app.name) && (
                          <div className="app-card-installed-badge">
                            {t("appsModal.installedBadge", "Installed")}
                          </div>
                        )}
                      </div>
                    ))
                  ) : (
                    <p>
                      {t(
                        "appsModal.noAppsFound",
                        "No apps found matching your search."
                      )}
                    </p>
                  )}
                </div>
              </>
            )}
          </>
        )}
      </div>
    </div>
  );
}

const getStorageAppName = () => {
  if (typeof window === 'undefined') return '';
  const urlForKey = window.location.href.split('#')[0];
  return urlForKey.replace(/[^a-zA-Z0-9.-_]/g, '_');
};
const storageAppName = getStorageAppName();
const getPrefixedKey = (key) => {
  const prefixedKey = `${storageAppName}_${key}`;
  if (displayId === 'display2' && PER_DISPLAY_SETTINGS.includes(key)) {
    return `${prefixedKey}_display2`;
  }
  return prefixedKey;
};

function Sidebar() {
  const [isOpen, setIsOpen] = useState(false);
  const [isToggleVisible, setIsToggleVisible] = useState(true);
  const toggleSidebar = () => {
    setIsOpen(!isOpen);
  };
  const isSecondaryDisplay = displayId === 'display2';
  const [langCode, setLangCode] = useState("en");
  const [translator, setTranslator] = useState(() => getTranslator("en"));
  useEffect(() => {
    window.postMessage({ type: 'sidebarVisibilityChanged', isOpen: isOpen }, window.location.origin);
  }, [isOpen]);
  const [currentDeviceDpi, setCurrentDeviceDpi] = useState(null);
  const [isMobile, setIsMobile] = useState(false);
  const [isTrackpadModeActive, setIsTrackpadModeActive] = useState(false);
  const [hasDetectedTouch, setHasDetectedTouch] = useState(false);
  const [heldKeys, setHeldKeys] = useState({
    Control: false,
    Alt: false,
    Meta: false,
  });
  const [isKeyboardButtonVisible, setIsKeyboardButtonVisible] = useState(true);
  const [isTouchGamepadActive, setIsTouchGamepadActive] = useState(false);
  const [isTouchGamepadSetup, setIsTouchGamepadSetup] = useState(false);
  const [availablePlacements, setAvailablePlacements] = useState(null);
  const [serverSettings, setServerSettings] = useState(null);
  const [renderableSettings, setRenderableSettings] = useState({});
  const [uiTitle, setUiTitle] = useState('Selkies');
  const [uiShowLogo, setUiShowLogo] = useState(true);

  useEffect(() => {
    const handleMessage = (event) => {
      if (
        event.origin === window.location.origin &&
        event.data?.type === "serverSettings"
      ) {
        console.log("Dashboard received server settings:", event.data.payload);
        setServerSettings(event.data.payload);
      }
    };
    window.addEventListener("message", handleMessage);
    return () => {
      window.removeEventListener("message", handleMessage);
    };
  }, []);

  useEffect(() => {
    if (!serverSettings) return;

    const newRenderable = {};
    const s = serverSettings;

    const isRenderable = (key) => {
        const setting = s[key];
        if (!setting) return true; 
        if (setting.locked === true) return false;
        if (setting.allowed && setting.allowed.length <= 1) return false;
        if (setting.min !== undefined && setting.max !== undefined && setting.min === setting.max) return false;
        return true;
    };

    newRenderable.videoSettings = s.ui_sidebar_show_video_settings?.value ?? true;
    newRenderable.screenSettings = s.ui_sidebar_show_screen_settings?.value ?? true;
    newRenderable.audioSettings = s.ui_sidebar_show_audio_settings?.value ?? true;
    newRenderable.stats = s.ui_sidebar_show_stats?.value ?? true;
    newRenderable.clipboard = s.ui_sidebar_show_clipboard?.value ?? true;
    newRenderable.files = s.ui_sidebar_show_files?.value ?? true;
    newRenderable.apps = s.ui_sidebar_show_apps?.value ?? true;
    newRenderable.sharing = s.ui_sidebar_show_sharing?.value ?? true;
    newRenderable.gamepads = s.ui_sidebar_show_gamepads?.value ?? true;
    newRenderable.fullscreen = s.ui_sidebar_show_fullscreen?.value ?? true;
    newRenderable.gamingMode = s.ui_sidebar_show_gaming_mode?.value ?? true;
    newRenderable.trackpad = s.ui_sidebar_show_trackpad?.value ?? true;
    newRenderable.keyboardButton = s.ui_sidebar_show_keyboard_button?.value ?? true;
    newRenderable.softButtons = s.ui_sidebar_show_soft_buttons?.value ?? true;
    newRenderable.coreButtons = s.ui_show_core_buttons?.value ?? true;

    newRenderable.encoder = isRenderable('encoder');
    newRenderable.framerate = isRenderable('framerate');
    newRenderable.jpeg_quality = isRenderable('jpeg_quality');
    newRenderable.paint_over_jpeg_quality = isRenderable('paint_over_jpeg_quality');
    newRenderable.h264_crf = isRenderable('h264_crf');
    newRenderable.h264PaintoverCRF = isRenderable('h264_paintover_crf');
    newRenderable.usePaintOverQuality = isRenderable('use_paint_over_quality');
    newRenderable.h264StreamingMode = isRenderable('h264_streaming_mode');
    newRenderable.h264FullColor = isRenderable('h264_fullcolor');
    newRenderable.use_cpu = isRenderable('use_cpu');
    newRenderable.uiScaling = isRenderable('scaling_dpi');
    newRenderable.binaryClipboard = isRenderable('enable_binary_clipboard');
    newRenderable.use_browser_cursors = isRenderable('use_browser_cursors');
    newRenderable.video_bitrate = isRenderable('video_bitrate');
    newRenderable.audio_bitrate = isRenderable('audio_bitrate');
    
    const hypotheticalHidpi = s.hidpi_enabled || { value: true, locked: false };
    newRenderable.hidpi = hypotheticalHidpi.locked !== true;

    newRenderable.enableSharing = s.enable_sharing?.value ?? true;
    newRenderable.enableShared = s.enable_shared?.value ?? true;
    newRenderable.enablePlayer2 = s.enable_player2?.value ?? true;
    newRenderable.enablePlayer3 = s.enable_player3?.value ?? true;
    newRenderable.enablePlayer4 = s.enable_player4?.value ?? true;
    newRenderable.enableDualMode = s.enable_dual_mode?.value ?? false;

    newRenderable.videoToggle = isRenderable('video_enabled');
    newRenderable.audioToggle = isRenderable('audio_enabled');
    newRenderable.microphoneToggle = isRenderable('microphone_enabled');
    newRenderable.gamepadToggle = isRenderable('gamepad_enabled');

    const ftSetting = s.file_transfers;
    newRenderable.fileUpload = ftSetting ? ftSetting.value.includes('upload') : true;
    newRenderable.fileDownload = ftSetting ? ftSetting.value.includes('download') : true;

    setRenderableSettings(newRenderable);
  }, [serverSettings]);

  const launchWindow = (direction, screen = null) => {
    const url = `${window.location.href.split('#')[0]}#display2-${direction}`;
    let features = 'resizable=yes,scrollbars=yes,noopener,noreferrer';
    if (screen) {
      features += `,left=${screen.availLeft},top=${screen.availTop},width=${screen.availWidth},height=${screen.availHeight}`;
    }
    window.open(url, '_blank', features);
    setAvailablePlacements(null);
  };

  const handleAddScreenClick = async () => {
    if (!('getScreenDetails' in window)) {
      console.warn("Window Management API not supported. Opening default second screen.");
      launchWindow('right');
      return;
    }

    try {
      const screenDetails = await window.getScreenDetails();
      const currentScreen = screenDetails.currentScreen;
      const otherScreens = screenDetails.screens.filter(s => s !== currentScreen);

      if (otherScreens.length === 0) {
        console.log("No other screens detected. Opening default second screen.");
        launchWindow('right');
        return;
      }

      const placements = {};
      for (const s of otherScreens) {
        if (!placements.right && s.left >= currentScreen.left + currentScreen.width) {
          placements.right = s;
        }
        if (!placements.left && s.left + s.width <= currentScreen.left) {
          placements.left = s;
        }
        if (!placements.down && s.top >= currentScreen.top + currentScreen.height) {
          placements.down = s;
        }
        if (!placements.up && s.top + s.height <= currentScreen.top) {
          placements.up = s;
        }
      }
      
      const availableDirections = Object.keys(placements);

      if (availableDirections.length === 1) {
        const direction = availableDirections[0];
        const screen = placements[direction];
        console.log(`Auto-placing single screen to the ${direction}.`);
        launchWindow(direction, screen);
      } else if (availableDirections.length > 1) {
        console.log("Multiple placement options found. Showing arrows.");
        setAvailablePlacements(placements);
      } else {
        console.log("No adjacent screens found in cardinal directions. Opening default.");
        launchWindow('right');
      }
    } catch (err) {
      console.error("Error with Window Management API or permission denied:", err);
      launchWindow('right');
    }
  };

  useEffect(() => {
    const browserLang = navigator.language || navigator.userLanguage || "en";
    const primaryLang = browserLang.split("-")[0].toLowerCase();
    console.log(
      `Dashboard: Detected browser language: ${browserLang}, using primary: ${primaryLang}`
    );
    setLangCode(primaryLang);
    setTranslator(getTranslator(primaryLang));
  }, []);

  useEffect(() => {
    const dpr = window.devicePixelRatio || 1;
    const targetDpi = dpr * 96;

    if (dpiScalingOptions && dpiScalingOptions.length > 0) {
      const closestOption = dpiScalingOptions.reduce((prev, curr) => {
        return Math.abs(curr.value - targetDpi) < Math.abs(prev.value - targetDpi)
          ? curr
          : prev;
      });
      setCurrentDeviceDpi(closestOption.value);
    }
  }, []);

  useEffect(() => {
    const mobileCheck =
      typeof window !== "undefined" &&
      ((navigator.userAgentData && navigator.userAgentData.mobile) ||
        /Mobi|Android|iPhone|iPad|iPod|BlackBerry|IEMobile|Opera Mini/i.test(
          navigator.userAgent
        ));
    setIsMobile(!!mobileCheck);

    if (!!mobileCheck) {
      setSectionsOpen((prev) => ({ ...prev, gamepads: true }));
    }

    if (
      navigator.userAgentData &&
      navigator.userAgentData.mobile !== undefined
    ) {
      console.log(
        "Dashboard: Mobile detected via userAgentData.mobile:",
        navigator.userAgentData.mobile
      );
    } else if (typeof navigator.userAgent === "string") {
      console.log(
        "Dashboard: Mobile detected via userAgent string match:",
        /Mobi|Android/i.test(navigator.userAgent)
      );
    } else {
      console.warn(
        "Dashboard: Mobile detection methods not fully available. Mobile status set to:",
        !!mobileCheck
      );
    }
  }, []);

  useEffect(() => {
    const detectTouch = () => {
      console.log("Dashboard: First touch detected. Enabling touch-specific features.");
      setHasDetectedTouch(true);
    };
    window.addEventListener('touchstart', detectTouch, { once: true, passive: true });
    return () => {
      window.removeEventListener('touchstart', detectTouch, { once: true, passive: true });
    };
  }, []);

  useEffect(() => {
    const setRealViewportHeight = () => {
      const vh = window.innerHeight * 0.01;
      document.documentElement.style.setProperty('--vh', `${vh}px`);
    };
    window.addEventListener('resize', setRealViewportHeight);
    window.addEventListener('orientationchange', setRealViewportHeight);
    setRealViewportHeight();
    return () => {
      window.removeEventListener('resize', setRealViewportHeight);
      window.removeEventListener('orientationchange', setRealViewportHeight);
    };
  }, []);

  useEffect(() => {
    if (!serverSettings) return;
    const getStoredInt = (key) => parseInt(localStorage.getItem(getPrefixedKey(key)), 10);
    const getStoredBool = (key) => localStorage.getItem(getPrefixedKey(key)) === 'true';
    const s_encoder = serverSettings.encoder;
    if (s_encoder) {
      const stored = localStorage.getItem(getPrefixedKey("encoder"));
      const final = s_encoder.allowed.includes(stored) ? stored : s_encoder.value;
      setEncoder(final);
      setDynamicEncoderOptions(s_encoder.allowed);
      localStorage.setItem(getPrefixedKey("encoder"), final);
    }
    const s_encoder_rtc = serverSettings.encoder_rtc;
    if (s_encoder_rtc) {
      const stored = localStorage.getItem(getPrefixedKey("encoder_rtc"));
      const final = s_encoder_rtc.allowed.includes(stored) ? stored : s_encoder_rtc.value;
      setEncoderRTC(final);
      setDynamicEncoderOptions(s_encoder_rtc.allowed);
      localStorage.setItem(getPrefixedKey("encoder_rtc"), final);
    }
    const s_framerate = serverSettings.framerate;
    if (s_framerate) {
      const stored = getStoredInt("framerate");
      const final = !isNaN(stored)
        ? Math.max(s_framerate.min, Math.min(s_framerate.max, stored))
        : s_framerate.default;
      setFramerate(final);
      localStorage.setItem(getPrefixedKey("framerate"), final);
    }
    const s_video_bitrate = serverSettings.video_bitrate;
    if (s_video_bitrate) {
      const stored = getStoredInt("video_bitrate");
      const final = !isNaN(stored)
        ? Math.max(s_video_bitrate.min, Math.min(s_video_bitrate.max, stored))
        : s_video_bitrate.default;
      setVideoBitrate(final);
      localStorage.setItem(getPrefixedKey("video_bitrate"), final);
    }
    const s_audio_bitrate = serverSettings.audio_bitrate;
    if (s_audio_bitrate) {
      const stored = getStoredInt("audio_bitrate");
      const final = s_audio_bitrate.allowed.includes(stored) ? stored : s_audio_bitrate.value;
      setAudioBitrate(final);
      localStorage.setItem(getPrefixedKey("audio_bitrate"), final);
    }
    const s_h264_crf = serverSettings.h264_crf;
    if (s_h264_crf) {
      const stored = getStoredInt("h264_crf");
      const final = !isNaN(stored)
        ? Math.max(s_h264_crf.min, Math.min(s_h264_crf.max, stored))
        : s_h264_crf.default;
      setVideoCRF(final);
      localStorage.setItem(getPrefixedKey("h264_crf"), final);
    }
    const s_jpeg_quality = serverSettings.jpeg_quality;
    if (s_jpeg_quality) {
      const stored = getStoredInt("jpeg_quality");
      const final = !isNaN(stored)
        ? Math.max(s_jpeg_quality.min, Math.min(s_jpeg_quality.max, stored))
        : s_jpeg_quality.default;
      setJpegQuality(final);
      localStorage.setItem(getPrefixedKey("jpeg_quality"), final);
    }
    const s_paint_over_jpeg_quality = serverSettings.paint_over_jpeg_quality;
    if (s_paint_over_jpeg_quality) {
      const stored = getStoredInt("paint_over_jpeg_quality");
      const final = !isNaN(stored)
        ? Math.max(s_paint_over_jpeg_quality.min, Math.min(s_paint_over_jpeg_quality.max, stored))
        : s_paint_over_jpeg_quality.default;
      setPaintOverJpegQuality(final);
      localStorage.setItem(getPrefixedKey("paint_over_jpeg_quality"), final);
    }
    const s_h264_paintover_crf = serverSettings.h264_paintover_crf;
    if (s_h264_paintover_crf) {
      const stored = getStoredInt("h264_paintover_crf");
      const final = !isNaN(stored)
        ? Math.max(s_h264_paintover_crf.min, Math.min(s_h264_paintover_crf.max, stored))
        : s_h264_paintover_crf.default;
      setH264PaintoverCRF(final);
      localStorage.setItem(getPrefixedKey("h264_paintover_crf"), final);
    }
    const s_use_paint_over_quality = serverSettings.use_paint_over_quality;
    if (s_use_paint_over_quality) {
      const stored = localStorage.getItem(getPrefixedKey("use_paint_over_quality"));
      const final = s_use_paint_over_quality.locked ? s_use_paint_over_quality.value : (stored !== null ? stored === 'true' : s_use_paint_over_quality.value);
      setUsePaintOverQuality(final);
      localStorage.setItem(getPrefixedKey("use_paint_over_quality"), String(final));
    }
    const s_h264_fullcolor = serverSettings.h264_fullcolor;
    if (s_h264_fullcolor) {
      const final = s_h264_fullcolor.locked ? s_h264_fullcolor.value : getStoredBool("h264_fullcolor");
      setH264FullColor(final);
      localStorage.setItem(getPrefixedKey("h264_fullcolor"), String(final));
    }
    const s_h264_streaming_mode = serverSettings.h264_streaming_mode;
    if (s_h264_streaming_mode) {
      const final = s_h264_streaming_mode.locked ? s_h264_streaming_mode.value : getStoredBool("h264_streaming_mode");
      setH264StreamingMode(final);
      localStorage.setItem(getPrefixedKey("h264_streaming_mode"), String(final));
    }
    const s_use_cpu = serverSettings.use_cpu;
    if (s_use_cpu) {
      const final = s_use_cpu.locked ? s_use_cpu.value : getStoredBool("use_cpu");
      setUseCpu(final);
      localStorage.setItem(getPrefixedKey("use_cpu"), String(final));
    }
    const s_scaling_dpi = serverSettings.scaling_dpi;
    if (s_scaling_dpi) {
      const stored = getStoredInt("scaling_dpi");
      const final = s_scaling_dpi.allowed.includes(String(stored)) ? stored : parseInt(s_scaling_dpi.value, 10);
      setSelectedDpi(final);
      localStorage.setItem(getPrefixedKey("scaling_dpi"), final);
    }
    const s_enable_binary_clipboard = serverSettings.enable_binary_clipboard;
    if (s_enable_binary_clipboard) {
      const final = s_enable_binary_clipboard.locked ? s_enable_binary_clipboard.value : getStoredBool("enable_binary_clipboard");
      setEnableBinaryClipboard(final);
      localStorage.setItem(getPrefixedKey("enable_binary_clipboard"), String(final));
    }
    const s_use_browser_cursors = serverSettings.use_browser_cursors;
    if (s_use_browser_cursors) {
      const final = s_use_browser_cursors.locked ? s_use_browser_cursors.value : getStoredBool("use_browser_cursors");
      setUseBrowserCursors(final);
    }
    const s_ui_title = serverSettings.ui_title;
    if (s_ui_title) {
        setUiTitle(s_ui_title.value);
    }
    const s_ui_show_logo = serverSettings.ui_show_logo;
    if (s_ui_show_logo) {
        setUiShowLogo(s_ui_show_logo.value);
    }
    const s_use_css_scaling = serverSettings.use_css_scaling;
    if (s_use_css_scaling) {
      const authoritativeValue = localStorage.getItem(getPrefixedKey("use_css_scaling")) === 'true';
      if (hidpiEnabled === authoritativeValue) {
        setHidpiEnabled(!authoritativeValue);
      }
    }
  }, [serverSettings]);

  const { t, raw } = translator;
  const sendKeyEvent = (type, key, code, modifierState) => {
    const event = new KeyboardEvent(type, {
      key: key,
      code: code,
      ctrlKey: modifierState.Control,
      altKey: modifierState.Alt,
      metaKey: modifierState.Meta,
      bubbles: true,
      cancelable: true,
    });
    window.dispatchEvent(event);
  };
  const handleHoldKeyClick = (key, code) => {
    const isCurrentlyHeld = heldKeys[key];
    const currentHeldCount = Object.values(heldKeys).filter(Boolean).length;
    if (!isCurrentlyHeld && currentHeldCount === 0) {
      window.postMessage({ type: 'setSynth', value: true }, window.location.origin);
    } else if (isCurrentlyHeld && currentHeldCount === 1) {
      window.postMessage({ type: 'setSynth', value: false }, window.location.origin);
    }
    const nextHeldState = {
      ...heldKeys,
      [key]: !isCurrentlyHeld,
    };
    setHeldKeys(nextHeldState);
    if (isCurrentlyHeld) {
      sendKeyEvent('keyup', key, code, nextHeldState);
      console.log(`Dashboard: Dispatched keyup for ${key} with state:`, nextHeldState);
    } else {
      sendKeyEvent('keydown', key, code, nextHeldState);
      console.log(`Dashboard: Dispatched keydown for ${key} with state:`, nextHeldState);
    }
  };
  const handleOnceKeyClick = (key, code) => {
    console.log(`Dashboard: Dispatching key press for ${key} with modifiers:`, heldKeys);
    sendKeyEvent('keydown', key, code, heldKeys);
    setTimeout(() => {
      sendKeyEvent('keyup', key, code, heldKeys);
    }, 50);
  };
  const toggleKeyboardButtonVisibility = () => {
    setIsKeyboardButtonVisible(prev => !prev);
  };

  const [streamMode, setStreamMode] = useState(
    localStorage.getItem(getPrefixedKey("stream_mode")) || DEFAULT_STREAM_MODE
  );
  const [encoderRTC, setEncoderRTC] = useState(
    localStorage.getItem(getPrefixedKey("encoder_rtc")) || DEFAULT_WEBRTC_ENCODER
  );
  const [dynamicEncoderOptions, setDynamicEncoderOptions] = useState();
  const [audioBitrate, setAudioBitrate] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("audio_bitrate")), 10) || DEFAULT_AUDIO_BITRATE
  );
  const [videoBitrate, setVideoBitrate] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("video_bitrate")), 10) || DEFAULT_VIDEO_BITRATE
  );
  const [theme, setTheme] = useState(localStorage.getItem("theme") || "dark");
  const [encoder, setEncoder] = useState(
    localStorage.getItem(getPrefixedKey("encoder")) || DEFAULT_ENCODER
  );
  const [framerate, setFramerate] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("framerate")), 10) ||
      DEFAULT_FRAMERATE
  );
  const [videoBufferSize, setVideoBufferSize] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("videoBufferSize")), 10) ||
      DEFAULT_VIDEO_BUFFER_SIZE
  );
  const [h264_crf, setVideoCRF] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("h264_crf")), 10) ||
      DEFAULT_VIDEO_CRF
  );
  const [h264PaintoverCRF, setH264PaintoverCRF] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("h264_paintover_crf")), 10) ||
      DEFAULT_H264_PAINTOVER_CRF
  );
  const [usePaintOverQuality, setUsePaintOverQuality] = useState(() => {
    const saved = localStorage.getItem(getPrefixedKey("use_paint_over_quality"));
    return saved !== null ? saved === 'true' : DEFAULT_USE_PAINT_OVER_QUALITY;
  });
  const [h264FullColor, setH264FullColor] = useState(
    localStorage.getItem(getPrefixedKey("h264_fullcolor")) === "true"
  );
  const [jpeg_quality, setJpegQuality] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("jpeg_quality")), 10) ||
      DEFAULT_JPEG_QUALITY
  );
  const [paint_over_jpeg_quality, setPaintOverJpegQuality] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("paint_over_jpeg_quality")), 10) ||
      DEFAULT_PAINT_OVER_JPEG_QUALITY
  );
  const [use_cpu, setUseCpu] = useState(
    localStorage.getItem(getPrefixedKey("use_cpu")) === "true"
  );
  const [h264StreamingMode, setH264StreamingMode] = useState(
    localStorage.getItem(getPrefixedKey("h264_streaming_mode")) === "true"
  );
  const [selectedDpi, setSelectedDpi] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("scaling_dpi")), 10) || DEFAULT_SCALING_DPI
  );
  const [manual_width, setManualWidth] = useState(localStorage.getItem(getPrefixedKey("manual_width")) || "");
  const [manual_height, setManualHeight] = useState(localStorage.getItem(getPrefixedKey("manual_height")) || "");
  const [scaleLocally, setScaleLocally] = useState(() => {
    const saved = localStorage.getItem(getPrefixedKey("scaleLocallyManual"));
    return saved !== null ? saved === "true" : DEFAULT_SCALE_LOCALLY;
  });
  const [hidpiEnabled, setHidpiEnabled] = useState(() => {
    const saved = localStorage.getItem(getPrefixedKey("use_css_scaling"));
    return saved !== "true";
  });
  const [antiAliasing, setAntiAliasing] = useState(() => {
    const saved = localStorage.getItem(getPrefixedKey("antiAliasingEnabled"));
    return saved !== null ? saved === "true" : true;
  });
  const [use_browser_cursors, setUseBrowserCursors] = useState(() => {
    const saved = localStorage.getItem(getPrefixedKey("use_browser_cursors"));
    return saved !== null ? saved === "true" : false;
  });
  const [enableBinaryClipboard, setEnableBinaryClipboard] = useState(() => {
    const saved = localStorage.getItem(getPrefixedKey("enable_binary_clipboard"));
    return saved !== null ? saved === 'true' : DEFAULT_ENABLE_BINARY_CLIPBOARD;
  });
  const [presetValue, setPresetValue] = useState("");
  const [clientFps, setClientFps] = useState(0);
  const [audioBuffer, setAudioBuffer] = useState(0);
  const [bandwidthMbps, setBandwidthMbps] = useState(0);
  const [latencyMs, setLatencyMs] = useState(0);
  const [cpuPercent, setCpuPercent] = useState(0);
  const [gpuPercent, setGpuPercent] = useState(0);
  const [sysMemPercent, setSysMemPercent] = useState(0);
  const [gpuMemPercent, setGpuMemPercent] = useState(0);
  const [sysMemUsed, setSysMemUsed] = useState(null);
  const [sysMemTotal, setSysMemTotal] = useState(null);
  const [gpuMemUsed, setGpuMemUsed] = useState(null);
  const [gpuMemTotal, setGpuMemTotal] = useState(null);
  const [hoveredItem, setHoveredItem] = useState(null);
  const [tooltipPosition, setTooltipPosition] = useState({ x: 0, y: 0 });
  const [isVideoActive, setIsVideoActive] = useState(true);
  const [isAudioActive, setIsAudioActive] = useState(true);
  const [isMicrophoneActive, setIsMicrophoneActive] = useState(false);
  const [isGamepadEnabled, setIsGamepadEnabled] = useState(true);
  const [dashboardClipboardContent, setDashboardClipboardContent] =
    useState("");
  const [audioInputDevices, setAudioInputDevices] = useState([]);
  const [audioOutputDevices, setAudioOutputDevices] = useState([]);
  const [selectedInputDeviceId, setSelectedInputDeviceId] = useState("default");
  const [selectedOutputDeviceId, setSelectedOutputDeviceId] =
    useState("default");
  const [isOutputSelectionSupported, setIsOutputSelectionSupported] =
    useState(false);
  const [audioDeviceError, setAudioDeviceError] = useState(null);
  const [isLoadingAudioDevices, setIsLoadingAudioDevices] = useState(false);
  const [gamepadStates, setGamepadStates] = useState({});
  const [hasReceivedGamepadData, setHasReceivedGamepadData] = useState(false);
  const [sectionsOpen, setSectionsOpen] = useState({
    settings: false,
    audioSettings: false,
    screenSettings: false,
    stats: false,
    clipboard: false,
    gamepads: false,
    files: false,
    apps: false,
    sharing: false,
  });
  const [notifications, setNotifications] = useState([]);
  const notificationTimeouts = useRef({});
  const [isFilesModalOpen, setIsFilesModalOpen] = useState(false);
  const [isAppsModalOpen, setIsAppsModalOpen] = useState(false);
  const [keyboardButtonPosition, setKeyboardButtonPosition] = useState({ bottom: 20, right: 20 });
  const dragInfo = useRef({
    isDragging: false,
    hasDragged: false,
    pointerId: null,
    startX: 0,
    startY: 0,
    initialBottom: 0,
    initialRight: 0,
  });


  // --- Debounce Settings ---
  const DEBOUNCE_DELAY = 500;

  const debouncedPostSetting = useCallback(
    debounce((setting) => {
      window.postMessage(
        { type: "settings", settings: setting },
        window.location.origin
      );
    }, DEBOUNCE_DELAY),
    []
  );

  const handleDpiScalingChange = (event) => {
    const newDpi = parseInt(event.target.value, 10);
    setSelectedDpi(newDpi);
    debouncedPostSetting({ scaling_dpi: newDpi });
  };

  const DRAG_THRESHOLD = 10;

  const handlePointerDown = (e) => {
    dragInfo.current.isDragging = true;
    dragInfo.current.hasDragged = false;
    dragInfo.current.pointerId = e.pointerId;
    dragInfo.current.startX = e.clientX;
    dragInfo.current.startY = e.clientY;
    dragInfo.current.initialBottom = keyboardButtonPosition.bottom;
    dragInfo.current.initialRight = keyboardButtonPosition.right;
    e.currentTarget.setPointerCapture(e.pointerId);
  };

  const handlePointerMove = (e) => {
    if (!dragInfo.current.isDragging) return;

    const dx = e.clientX - dragInfo.current.startX;
    const dy = e.clientY - dragInfo.current.startY;

    if (!dragInfo.current.hasDragged && (Math.abs(dx) > DRAG_THRESHOLD || Math.abs(dy) > DRAG_THRESHOLD)) {
      dragInfo.current.hasDragged = true;
    }

    if (dragInfo.current.hasDragged) {
      setKeyboardButtonPosition({
        bottom: dragInfo.current.initialBottom - dy,
        right: dragInfo.current.initialRight - dx,
      });
    }
  };

  const handlePointerUp = (e) => {
    if (e.currentTarget.hasPointerCapture(dragInfo.current.pointerId)) {
      e.currentTarget.releasePointerCapture(e.pointerId);
    }
    dragInfo.current.isDragging = false;
    dragInfo.current.pointerId = null;
  };

  const onKeyboardButtonClick = (e) => {
    if (dragInfo.current.hasDragged) {
      e.preventDefault();
      e.stopPropagation();
      dragInfo.current.hasDragged = false;
      return;
    }
    handleShowVirtualKeyboard();
  };

  const toggleAppsModal = () => setIsAppsModalOpen(!isAppsModalOpen);
  const toggleFilesModal = () => setIsFilesModalOpen(!isFilesModalOpen);
  const handleShowVirtualKeyboard = useCallback(() => {
    console.log("Dashboard: Directly handling virtual keyboard pop.");
    const kbdAssistInput = document.getElementById('keyboard-input-assist');
    const mainInteractionOverlay = document.getElementById('overlayInput');
    if (kbdAssistInput) {
      kbdAssistInput.removeAttribute('aria-hidden');
      kbdAssistInput.value = '';
      kbdAssistInput.focus();
      console.log("Focused #keyboard-input-assist element to pop keyboard.");
      if (mainInteractionOverlay) {
        mainInteractionOverlay.addEventListener(
          "touchstart",
          () => {
            if (document.activeElement === kbdAssistInput) {
              kbdAssistInput.blur();
              console.log("Blurred #keyboard-input-assist on main overlay touch.");
              kbdAssistInput.setAttribute('aria-hidden', 'true');
            }
          }, {
            once: true,
            passive: true
          }
        );
      } else {
         console.warn("Could not find #overlayInput to attach blur listener.");
      }
    } else {
      console.error("Could not find #keyboard-input-assist element to focus.");
    }
  }, []);

  const populateAudioDevices = useCallback(async () => {
    console.log("Dashboard: Attempting to populate audio devices...");
    setIsLoadingAudioDevices(true);
    setAudioDeviceError(null);
    setAudioInputDevices([]);
    setAudioOutputDevices([]);
    const supportsSinkId = "setSinkId" in HTMLMediaElement.prototype;
    setIsOutputSelectionSupported(supportsSinkId);
    console.log(
      "Dashboard: Output device selection supported:",
      supportsSinkId
    );
    try {
      console.log(
        "Dashboard: Requesting temporary microphone permission for device listing..."
      );
      const tempStream = await navigator.mediaDevices.getUserMedia({
        audio: true,
      });
      tempStream.getTracks().forEach((track) => track.stop());
      console.log("Dashboard: Temporary permission granted/available.");
      console.log("Dashboard: Enumerating media devices...");
      const devices = await navigator.mediaDevices.enumerateDevices();
      console.log("Dashboard: Devices found:", devices);
      const inputs = [];
      const outputs = [];
      devices.forEach((device, index) => {
        if (!device.deviceId) {
          console.warn(
            "Dashboard: Skipping device with missing deviceId:",
            device
          );
          return;
        }
        const label =
          device.label ||
          (device.kind === "audioinput"
            ? t("sections.audio.defaultInputLabelFallback", {
                index: index + 1,
              })
            : t("sections.audio.defaultOutputLabelFallback", {
                index: index + 1,
              }));
        if (device.kind === "audioinput") {
          inputs.push({ deviceId: device.deviceId, label: label });
        } else if (device.kind === "audiooutput" && supportsSinkId) {
          outputs.push({ deviceId: device.deviceId, label: label });
        }
      });
      setAudioInputDevices(inputs);
      setAudioOutputDevices(outputs);
      setSelectedInputDeviceId("default");
      setSelectedOutputDeviceId("default");
      console.log(
        `Dashboard: Populated ${inputs.length} inputs, ${outputs.length} outputs.`
      );
    } catch (err) {
      console.error(
        "Dashboard: Error getting media devices or permissions:",
        err
      );
      let userMessageKey = "sections.audio.deviceErrorDefault";
      let errorVars = { errorName: err.name || "Unknown error" };
      if (err.name === "NotAllowedError")
        userMessageKey = "sections.audio.deviceErrorPermission";
      else if (err.name === "NotFoundError")
        userMessageKey = "sections.audio.deviceErrorNotFound";
      setAudioDeviceError(t(userMessageKey, errorVars));
    } finally {
      setIsLoadingAudioDevices(false);
    }
  }, [t]);

  const toggleSection = useCallback(
    (sectionKey) => {
      const isOpening = !sectionsOpen[sectionKey];
      setSectionsOpen((prev) => ({ ...prev, [sectionKey]: !prev[sectionKey] }));
      if (sectionKey === "audioSettings" && isOpening) {
        populateAudioDevices();
      }
    },
    [sectionsOpen, populateAudioDevices]
  );
  const baseUrl = typeof window !== 'undefined' ? window.location.href.split('#')[0] : '';
  const sharingLinks = [
    {
      id: "shared",
      label: "Read only viewer",
      tooltip: "Read only client for viewing, as many clients as needed can connect to this endpoint and see the live session",
      hash: "#shared",
    },
    {
      id: "player2",
      label: "Controller 2",
      tooltip: "Player 2 gamepad input, this endpoint has full control over the player 2 gamepad",
      hash: "#player2",
    },
    {
      id: "player3",
      label: "Controller 3",
      tooltip: "Player 3 gamepad input, this endpoint has full control over the player 3 gamepad",
      hash: "#player3",
    },
    {
      id: "player4",
      label: "Controller 4",
      tooltip: "Player 4 gamepad input, this endpoint has full control over the player 4 gamepad",
      hash: "#player4",
    },
  ];
  const handleCopyLink = async (textToCopy, label) => {
    if (!navigator.clipboard) {
      console.warn("Clipboard API not available.");
      return;
    }
    try {
      await navigator.clipboard.writeText(textToCopy);
      const id = `copy-success-${label.toLowerCase().replace(/\s+/g, '-')}`;
      setNotifications(prev => {
        const filtered = prev.filter(n => n.id !== id);
        const newNotifs = [...filtered, {
          id,
          fileName: t("notifications.copiedTitle", { label: label }),
          status: 'end',
          message: t("notifications.copiedMessage", { textToCopy: textToCopy }),
          timestamp: Date.now(),
          fadingOut: false,
        }];
        return newNotifs.slice(-MAX_NOTIFICATIONS);
      });
      scheduleNotificationRemoval(id, NOTIFICATION_TIMEOUT_SUCCESS);
    } catch (err) {
      console.error("Failed to copy link: ", err);
      const id = `copy-error-${label.toLowerCase().replace(/\s+/g, '-')}`;
      setNotifications(prev => {
        const filtered = prev.filter(n => n.id !== id);
        const newNotifs = [...filtered, {
          id,
          fileName: t("notifications.copyFailedTitle", { label: label }),
          status: 'error',
          message: t('notifications.copyFailedError'),
          timestamp: Date.now(),
          fadingOut: false,
        }];
        return newNotifs.slice(-MAX_NOTIFICATIONS);
      });
      scheduleNotificationRemoval(id, NOTIFICATION_TIMEOUT_ERROR);
    }
  };
  const handleEncoderChange = (event) => {
    const selectedEncoder = event.target.value;
    if (streamMode === STREAM_MODE_WEBRTC) {
      setEncoderRTC(selectedEncoder);
    } else {
      setEncoder(selectedEncoder);
      }
    debouncedPostSetting({ encoder: selectedEncoder });
  };
  const handleFramerateChange = (event) => {
    const selectedFramerate = parseInt(event.target.value, 10);
    setFramerate(selectedFramerate);
    debouncedPostSetting({ framerate: selectedFramerate });
  };
  const handleVideoBitrateChange = (event) => {
    const selectedVideoBitrate = parseInt(event.target.value, 10);
    setVideoBitrate(selectedVideoBitrate)
    debouncedPostSetting({ video_bitrate: selectedVideoBitrate})
  };
  const handleAudioBitrateChange = (event) => {
    const selectedAudioBitrate = parseInt(event.target.value, 10);
    setAudioBitrate(selectedAudioBitrate)
    debouncedPostSetting({ audio_bitrate: selectedAudioBitrate})
  }
  const handleJpegQualityChange = (event) => {
    const selectedQuality = parseInt(event.target.value, 10);
    setJpegQuality(selectedQuality);
    debouncedPostSetting({ jpeg_quality: selectedQuality });
  };
  const handlePaintOverJpegQualityChange = (event) => {
    const selectedQuality = parseInt(event.target.value, 10);
    setPaintOverJpegQuality(selectedQuality);
    debouncedPostSetting({ paint_over_jpeg_quality: selectedQuality });
  };
  const handleVideoCRFChange = (event) => {
    const selectedCRF = parseInt(event.target.value, 10);
    setVideoCRF(selectedCRF);
    debouncedPostSetting({ h264_crf: selectedCRF });
  };
  const handleH264PaintoverCRFChange = (event) => {
    const selectedCRF = parseInt(event.target.value, 10);
    setH264PaintoverCRF(selectedCRF);
    debouncedPostSetting({ h264_paintover_crf: selectedCRF });
  };
  const handleH264FullColorToggle = () => {
    const newFullColorState = !h264FullColor;
    setH264FullColor(newFullColorState);
    debouncedPostSetting({ h264_fullcolor: newFullColorState });
  };
  const handleUsePaintOverQualityToggle = () => {
    const newUsePaintOverQualityState = !usePaintOverQuality;
    setUsePaintOverQuality(newUsePaintOverQualityState);
    debouncedPostSetting({ use_paint_over_quality: newUsePaintOverQualityState });
  };
  const handleUseCpuToggle = () => {
    const newUseCpuState = !use_cpu;
    setUseCpu(newUseCpuState);
    debouncedPostSetting({ use_cpu: newUseCpuState });
  };
  const handleH264StreamingModeToggle = () => {
    const newStreamingModeState = !h264StreamingMode;
    setH264StreamingMode(newStreamingModeState);
    debouncedPostSetting({ h264_streaming_mode: newStreamingModeState });
  };
  const handleAudioInputChange = (event) => {
    const deviceId = event.target.value;
    setSelectedInputDeviceId(deviceId);
    window.postMessage(
      { type: "audioDeviceSelected", context: "input", deviceId: deviceId },
      window.location.origin
    );
  };
  const handleAudioOutputChange = (event) => {
    const deviceId = event.target.value;
    setSelectedOutputDeviceId(deviceId);
    window.postMessage(
      { type: "audioDeviceSelected", context: "output", deviceId: deviceId },
      window.location.origin
    );
  };
  const handlePresetChange = (event) => {
    const selectedValue = event.target.value;
    setPresetValue(selectedValue);
    if (!selectedValue) return;
    const parts = selectedValue.split("x");
    if (parts.length === 2) {
      const width = parseInt(parts[0], 10),
        height = parseInt(parts[1], 10);
      if (!isNaN(width) && width > 0 && !isNaN(height) && height > 0) {
        const evenWidth = roundDownToEven(width),
          evenHeight = roundDownToEven(height);
        setManualWidth(evenWidth.toString());
        setManualHeight(evenHeight.toString());
        window.postMessage(
          { type: "setManualResolution", width: evenWidth, height: evenHeight },
          window.location.origin
        );
      } else
        console.error(
          "Dashboard: Error parsing selected resolution preset:",
          selectedValue
        );
    }
  };
  const handleManualWidthChange = (event) => {
    setManualWidth(event.target.value);
    setPresetValue("");
  };
  const handleManualHeightChange = (event) => {
    setManualHeight(event.target.value);
    setPresetValue("");
  };
  const handleScaleLocallyToggle = () => {
    const newState = !scaleLocally;
    setScaleLocally(newState);
    window.postMessage(
      { type: "setScaleLocally", value: newState },
      window.location.origin
    );
  };
  const handleHidpiToggle = () => {
    const newHidpiState = !hidpiEnabled;
    setHidpiEnabled(newHidpiState);
    window.postMessage(
      { type: "setUseCssScaling", value: !newHidpiState },
      window.location.origin
    );
  };
  const handleAntiAliasingToggle = () => {
    const newState = !antiAliasing;
    setAntiAliasing(newState);
    window.postMessage(
      { type: "setAntiAliasing", value: newState },
      window.location.origin
    );
  };
  const handleUseBrowserCursorsToggle = () => {
    const newState = !use_browser_cursors;
    setUseBrowserCursors(newState);
    window.postMessage(
      { type: "setUseBrowserCursors", value: newState },
      window.location.origin
    );
  };
  const handleEnableBinaryClipboardToggle = () => {
    const newState = !enableBinaryClipboard;
    setEnableBinaryClipboard(newState);
    debouncedPostSetting({ enable_binary_clipboard: newState });
  };
  const handleSetManualResolution = () => {
    const width = parseInt(manual_width.trim(), 10),
      height = parseInt(manual_height.trim(), 10);
    if (isNaN(width) || width <= 0 || isNaN(height) || height <= 0) {
      alert(t("alerts.invalidResolution"));
      return;
    }
    const evenWidth = roundDownToEven(width),
      evenHeight = roundDownToEven(height);
    setManualWidth(evenWidth.toString());
    setManualHeight(evenHeight.toString());
    setPresetValue("");
    window.postMessage(
      { type: "setManualResolution", width: evenWidth, height: evenHeight },
      window.location.origin
    );
  };
  const handleResetResolution = () => {
    setManualWidth("");
    setManualHeight("");
    setPresetValue("");
    window.postMessage(
      { type: "resetResolutionToWindow" },
      window.location.origin
    );
  };
  const handleVideoToggle = () =>
    window.postMessage(
      { type: "pipelineControl", pipeline: "video", enabled: !isVideoActive },
      window.location.origin
    );
  const handleAudioToggle = () =>
    window.postMessage(
      { type: "pipelineControl", pipeline: "audio", enabled: !isAudioActive },
      window.location.origin
    );
  const handleMicrophoneToggle = () =>
    window.postMessage(
      {
        type: "pipelineControl",
        pipeline: "microphone",
        enabled: !isMicrophoneActive,
      },
      window.location.origin
    );
  const handleGamepadToggle = () =>
    window.postMessage(
      { type: "gamepadControl", enabled: !isGamepadEnabled },
      window.location.origin
    );
  const handleFullscreenRequest = () => {
    if (document.fullscreenElement) {
      if (document.exitFullscreen) {
        document.exitFullscreen().catch(err => console.error("Error exiting fullscreen:", err));
      }
    } else {
      window.postMessage({ type: "requestFullscreen" }, window.location.origin);
    }
  };
  const handleBrowserFullscreen = () => {
    if (!document.fullscreenElement) {
      const elem = document.documentElement;
      if (elem.requestFullscreen) {
        elem.requestFullscreen().catch(err => {
          console.error(`Error attempting to enable full-screen mode: ${err.message} (${err.name})`);
        });
      } else if (elem.mozRequestFullScreen) { /* Firefox */
        elem.mozRequestFullScreen();
      } else if (elem.webkitRequestFullscreen) { /* Chrome, Safari & Opera */
        elem.webkitRequestFullscreen();
      } else if (elem.msRequestFullscreen) { /* IE/Edge */
        elem.msRequestFullscreen();
      }
    } else {
      if (document.exitFullscreen) {
        document.exitFullscreen().catch(err => console.error("Error exiting fullscreen:", err));
      } else if (document.mozCancelFullScreen) { /* Firefox */
        document.mozCancelFullScreen();
      } else if (document.webkitExitFullscreen) { /* Chrome, Safari and Opera */
        document.webkitExitFullscreen();
      } else if (document.msExitFullscreen) { /* IE/Edge */
        document.msExitFullscreen();
      }
    }
  };
  const handleClipboardChange = (event) =>
    setDashboardClipboardContent(event.target.value);
  const handleClipboardBlur = (event) =>
    window.postMessage(
      { type: "clipboardUpdateFromUI", text: event.target.value },
      window.location.origin
    );
  const toggleTheme = () => {
    const newTheme = theme === "dark" ? "light" : "dark";
    setTheme(newTheme);
  };
  const handleStreamModeChange = async (event) => {
    const newMode = event.target.value;
    console.log("Change of stream mode requested:", newMode);
    try {
      const response = await fetch("/switch", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
        },
        body: JSON.stringify({ mode: newMode }),
      });

      if (!response.ok) {
        throw new Error(`Request failed with status ${response.status}`);
      }
      const data = await response.json();
      setStreamMode(newMode);
      window.postMessage(
        { type: "mode", mode: newMode },
        window.location.origin
      );
    } catch (error) {
        console.error("Error switching stream mode:", error);
    }
  }
  const handleMouseEnter = (e, itemKey) => {
    setHoveredItem(itemKey);
    setTooltipPosition({ x: e.clientX + 10, y: e.clientY + 10 });
  };
  const handleMouseLeave = () => setHoveredItem(null);

  const handleToggleTouchGamepad = useCallback(() => {
    const newActiveState = !isTouchGamepadActive;
    setIsTouchGamepadActive(newActiveState);

    if (newActiveState && !isTouchGamepadSetup) {
      window.postMessage(
        {
          type: "TOUCH_GAMEPAD_SETUP",
          payload: { targetDivId: TOUCH_GAMEPAD_HOST_DIV_ID, visible: true },
        },
        window.location.origin
      );
      setIsTouchGamepadSetup(true);
      console.log(
        "Dashboard: Touch Gamepad SETUP sent, targetDivId:",
        TOUCH_GAMEPAD_HOST_DIV_ID,
        "visible: true"
      );
    } else if (isTouchGamepadSetup) {
      window.postMessage(
        {
          type: "TOUCH_GAMEPAD_VISIBILITY",
          payload: {
            visible: newActiveState,
            targetDivId: TOUCH_GAMEPAD_HOST_DIV_ID,
          },
        },
        window.location.origin
      );
      console.log(
        `Dashboard: Touch Gamepad VISIBILITY sent, targetDivId:`,
        TOUCH_GAMEPAD_HOST_DIV_ID,
        `visible: ${newActiveState}`
      );
    }
  }, [isTouchGamepadActive, isTouchGamepadSetup]);

  const handleToggleTrackpadMode = useCallback(() => {
    const newActiveState = !isTrackpadModeActive;
    setIsTrackpadModeActive(newActiveState);
    const message = newActiveState ? "touchinput:trackpad" : "touchinput:touch";
    console.log(`Dashboard: Toggling trackpad mode. Sending: ${message}`);
    window.postMessage({ type: message }, window.location.origin);
  }, [isTrackpadModeActive]);

  const getTooltipContent = useCallback(
    (itemKey) => {
      const memNA = t("sections.stats.tooltipMemoryNA");
      switch (itemKey) {
        case "cpu":
          return t("sections.stats.tooltipCpu", {
            value: cpuPercent.toFixed(1),
          });
        case "gpu":
          return t("sections.stats.tooltipGpu", {
            value: gpuPercent.toFixed(1),
          });
        case "sysmem":
          const fu =
            sysMemUsed !== null ? formatBytes(sysMemUsed, 2, raw) : memNA;
          const ft =
            sysMemTotal !== null ? formatBytes(sysMemTotal, 2, raw) : memNA;
          return fu !== memNA && ft !== memNA
            ? t("sections.stats.tooltipSysMem", { used: fu, total: ft })
            : `${t("sections.stats.sysMemLabel")}: ${memNA}`;
        case "gpumem":
          const gu =
            gpuMemUsed !== null ? formatBytes(gpuMemUsed, 2, raw) : memNA;
          const gt =
            gpuMemTotal !== null ? formatBytes(gpuMemTotal, 2, raw) : memNA;
          return gu !== memNA && gt !== memNA
            ? t("sections.stats.tooltipGpuMem", { used: gu, total: gt })
            : `${t("sections.stats.gpuMemLabel")}: ${memNA}`;
        case "fps":
          return t("sections.stats.tooltipFps", { value: clientFps });
        case "audio":
          return t("sections.stats.tooltipAudio", { value: audioBuffer });
        case "bandwidth":
          return t("sections.stats.tooltipBandwidth", { value: bandwidthMbps.toFixed(2) }, `Bandwidth: ${bandwidthMbps.toFixed(2)} Mbps`);
        case "latency":
          return t("sections.stats.tooltipLatency", { value: latencyMs.toFixed(1) }, `Latency: ${latencyMs.toFixed(1)} ms`);
        default:
          return "";
      }
    },
    [
      t,
      raw,
      cpuPercent,
      gpuPercent,
      sysMemUsed,
      sysMemTotal,
      gpuMemUsed,
      gpuMemTotal,
      clientFps,
      audioBuffer,
    ]
  );

  const removeNotification = useCallback((id) => {
    setNotifications((prev) => prev.filter((n) => n.id !== id));
    if (notificationTimeouts.current[id]) {
      clearTimeout(notificationTimeouts.current[id].fadeTimer);
      clearTimeout(notificationTimeouts.current[id].removeTimer);
      delete notificationTimeouts.current[id];
    }
  }, []);

  const scheduleNotificationRemoval = useCallback(
    (id, delay) => {
      if (notificationTimeouts.current[id]) {
        clearTimeout(notificationTimeouts.current[id].fadeTimer);
        clearTimeout(notificationTimeouts.current[id].removeTimer);
      }
      const fadeTimer = setTimeout(
        () =>
          setNotifications((prev) =>
            prev.map((n) => (n.id === id ? { ...n, fadingOut: true } : n))
          ),
        delay - NOTIFICATION_FADE_DURATION
      );
      const removeTimer = setTimeout(() => removeNotification(id), delay);
      notificationTimeouts.current[id] = { fadeTimer, removeTimer };
    },
    [removeNotification]
  );

  const handleUploadClick = () =>
    window.dispatchEvent(new CustomEvent("requestFileUpload"));

  useEffect(() => {
    const readStats = () => {
      const cs = window.system_stats,
        su = cs?.mem_used ?? null,
        st = cs?.mem_total ?? null;
      setCpuPercent(cs?.cpu_percent ?? 0);
      setSysMemUsed(su);
      setSysMemTotal(st);
      setSysMemPercent(
        su !== null && st !== null && st > 0 ? (su / st) * 100 : 0
      );
      const cgs = window.gpu_stats,
        gp = cgs?.gpu_percent ?? cgs?.utilization_gpu ?? 0;
      setGpuPercent(gp);
      const gu =
        cgs?.mem_used ?? cgs?.memory_used ?? cgs?.used_gpu_memory_bytes ?? null;
      const gt =
        cgs?.mem_total ??
        cgs?.memory_total ??
        cgs?.total_gpu_memory_bytes ??
        null;
      setGpuMemUsed(gu);
      setGpuMemTotal(gt);
      setGpuMemPercent(
        gu !== null && gt !== null && gt > 0 ? (gu / gt) * 100 : 0
      );
      setClientFps(window.fps ?? 0);
      setAudioBuffer(window.currentAudioBufferSize ?? 0);
      const netStats = window.network_stats;
      setBandwidthMbps(netStats?.bandwidth_mbps ?? 0);
      setLatencyMs(netStats?.latency_ms ?? 0);
    };
    const intervalId = setInterval(readStats, STATS_READ_INTERVAL_MS);
    return () => clearInterval(intervalId);
  }, [isOpen]);

  useEffect(() => {
    const handleWindowMessage = (event) => {
      if (event.origin !== window.location.origin) return;
      const message = event.data;
      if (typeof message === "object" && message !== null) {
        if (message.type === "pipelineStatusUpdate") {
          if (message.video !== undefined) setIsVideoActive(message.video);
          if (message.audio !== undefined) setIsAudioActive(message.audio);
          if (message.microphone !== undefined)
            setIsMicrophoneActive(message.microphone);
        } else if (message.type === 'clientRoleUpdate' && message.role === 'viewer') {
          setIsToggleVisible(false);
        } else if (message.type === "gamepadControl") {
          if (message.enabled !== undefined)
            setIsGamepadEnabled(message.enabled);
        } else if (message.type === "sidebarButtonStatusUpdate") {
          if (message.video !== undefined) setIsVideoActive(message.video);
          if (message.audio !== undefined) setIsAudioActive(message.audio);
          if (message.microphone !== undefined)
            setIsMicrophoneActive(message.microphone);
          if (message.gamepad !== undefined)
            setIsGamepadEnabled(message.gamepad);
        } else if (message.type === "clipboardContentUpdate") {
          if (typeof message.text === "string")
            setDashboardClipboardContent(message.text);
        } else if (message.type === "audioDeviceStatusUpdate") {
          if (message.inputDeviceId !== undefined)
            setSelectedInputDeviceId(message.inputDeviceId || "default");
          if (message.outputDeviceId !== undefined)
            setSelectedOutputDeviceId(message.outputDeviceId || "default");
        } else if (
          message.type === "gamepadButtonUpdate" ||
          message.type === "gamepadAxisUpdate"
        ) {
          if (!hasReceivedGamepadData) setHasReceivedGamepadData(true);
          const gpIndex = message.gamepadIndex;
          if (gpIndex === undefined || gpIndex === null) return;
          setGamepadStates((prev) => {
            const ns = { ...prev };
            if (!ns[gpIndex]) ns[gpIndex] = { buttons: {}, axes: {} };
            else
              ns[gpIndex] = {
                buttons: { ...(ns[gpIndex].buttons || {}) },
                axes: { ...(ns[gpIndex].axes || {}) },
              };
            if (message.type === "gamepadButtonUpdate")
              ns[gpIndex].buttons[message.buttonIndex] = message.value || 0;
            else
              ns[gpIndex].axes[message.axisIndex] = Math.max(
                -1,
                Math.min(1, message.value || 0)
              );
            return ns;
          });
        } else if (message.type === "fileUpload") {
          const {
            status,
            fileName,
            progress,
            fileSize,
            message: errMsg,
          } = message.payload;
          const id = fileName;
          setNotifications((prev) => {
            const exIdx = prev.findIndex((n) => n.id === id);
            if (exIdx === -1) {
              if (prev.length < MAX_NOTIFICATIONS && status === "start")
                return [
                  ...prev,
                  {
                    id,
                    fileName,
                    status: "progress",
                    progress: 0,
                    fileSize,
                    message: null,
                    timestamp: Date.now(),
                    fadingOut: false,
                  },
                ];
              if (prev.length < MAX_NOTIFICATIONS && status === "warning") {
                scheduleNotificationRemoval(id, NOTIFICATION_TIMEOUT_SUCCESS);
                return [
                  ...prev,
                  {
                    id,
                    fileName: "Warning",
                    status: "warn",
                    message: errMsg,
                    timestamp: Date.now(),
                    fadingOut: false,
                  }
                ];
              } else return prev;
            } else if (exIdx !== -1) {
              const un = [...prev],
                cn = un[exIdx];
              if (notificationTimeouts.current[id]) {
                clearTimeout(notificationTimeouts.current[id].fadeTimer);
                clearTimeout(notificationTimeouts.current[id].removeTimer);
                delete notificationTimeouts.current[id];
              }
              if (status === "progress")
                un[exIdx] = {
                  ...cn,
                  status: "progress",
                  progress,
                  timestamp: Date.now(),
                  fadingOut: false,
                };
              else if (status === "end") {
                un[exIdx] = {
                  ...cn,
                  status: "end",
                  progress: 100,
                  message: null,
                  timestamp: Date.now(),
                  fadingOut: false,
                };
                scheduleNotificationRemoval(id, NOTIFICATION_TIMEOUT_SUCCESS);
              } else if (status === "error") {
                const te = errMsg
                  ? `${t("notifications.errorPrefix")} ${errMsg}`
                  : t("notifications.unknownError");
                un[exIdx] = {
                  ...cn,
                  status: "error",
                  progress: 100,
                  message: te,
                  timestamp: Date.now(),
                  fadingOut: false,
                };
                scheduleNotificationRemoval(id, NOTIFICATION_TIMEOUT_ERROR);
              } else if (status === "warning") {
                  un[exIdx] = {
                    ...cn,
                    fileName: "Warning",
                    status: "warn",
                    message: errMsg,
                    timestamp: Date.now(),
                    fadingOut: false,
                  };
                  scheduleNotificationRemoval(id, NOTIFICATION_TIMEOUT_ERROR);
              }
              return un;
            } else return prev;
          });
        } else if (message.type === "serverSettings") {
          if (message.encoders && Array.isArray(message.encoders)) {
            const newEncoderOptions =
              Array.isArray(message.encoders) && message.encoders.length > 0
                ? message.encoders
                : encoderOptions;
            setDynamicEncoderOptions(newEncoderOptions);
          }
          if (typeof message.enableBinaryClipboard === 'boolean') {
            setEnableBinaryClipboard(message.enableBinaryClipboard);
            console.log("Dashboard: Received enableBinaryClipboard setting from server:", message.enableBinaryClipboard);
          }
        } else if (message.type === "trackpadModeUpdate") {
          if (typeof message.enabled === 'boolean') {
            setIsTrackpadModeActive(message.enabled);
          }
        }
      }
    };
    window.addEventListener("message", handleWindowMessage);
    return () => {
      window.removeEventListener("message", handleWindowMessage);
      Object.values(notificationTimeouts.current).forEach((timers) => {
        clearTimeout(timers.fadeTimer);
        clearTimeout(timers.removeTimer);
      });
      notificationTimeouts.current = {};
    };
  }, [
    hasReceivedGamepadData,
    scheduleNotificationRemoval,
    removeNotification,
    t,
    dynamicEncoderOptions,
    isOpen,
  ]);

  const gaugeSize = 80,
    gaugeStrokeWidth = 8,
    gaugeRadius = gaugeSize / 2 - gaugeStrokeWidth / 2;
  const gaugeCircumference = 2 * Math.PI * gaugeRadius,
    gaugeCenter = gaugeSize / 2;
  const cpuOffset = calculateGaugeOffset(
    cpuPercent,
    gaugeRadius,
    gaugeCircumference
  );
  const gpuOffset = calculateGaugeOffset(
    gpuPercent,
    gaugeRadius,
    gaugeCircumference
  );
  const sysMemOffset = calculateGaugeOffset(
    sysMemPercent,
    gaugeRadius,
    gaugeCircumference
  );
  const gpuMemOffset = calculateGaugeOffset(
    gpuMemPercent,
    gaugeRadius,
    gaugeCircumference
  );
  const fpsPercent = Math.min(
    100,
    (clientFps / (framerate || DEFAULT_FRAMERATE)) * 100
  );
  const fpsOffset = calculateGaugeOffset(
    fpsPercent,
    gaugeRadius,
    gaugeCircumference
  );
  const audioBufferPercent = Math.min(
    100,
    (audioBuffer / MAX_AUDIO_BUFFER) * 100
  );
  const audioBufferOffset = calculateGaugeOffset(
    audioBufferPercent,
    gaugeRadius,
    gaugeCircumference
  );
  const MAX_BANDWIDTH_MBPS = 1000;
  const MAX_LATENCY_MS = 1000;
  const bandwidthPercent = Math.min(100, (bandwidthMbps / MAX_BANDWIDTH_MBPS) * 100);
  const bandwidthOffset = calculateGaugeOffset(
    bandwidthPercent,
    gaugeRadius,
    gaugeCircumference
  );
  const latencyPercent = Math.min(100, (latencyMs / MAX_LATENCY_MS) * 100);
  const latencyOffset = calculateGaugeOffset(
    latencyPercent,
    gaugeRadius,
    gaugeCircumference
  );
  const translatedCommonResolutions = commonResolutionValues.map(
    (value, index) => ({
      value: value,
      text:
        index === 0
          ? t("sections.screen.resolutionPresetSelect")
          : raw?.resolutionPresets?.[value] || value,
    })
  );

  const showFPS = [
    "jpeg",
    "x264enc-striped",
    "x264enc",
  ].includes(encoder);
  const showBitrate = [
  ].includes(encoder);
  const showBufferSize = [
  ].includes(encoder);
  const showCRF = ["x264enc-striped", "x264enc"].includes(encoder);
  const showH264Options = ["x264enc-striped", "x264enc"].includes(encoder);
  const showJpegOptions = encoder === 'jpeg';
  const showPaintOverQualityToggle = showH264Options || showJpegOptions;
  if (serverSettings && serverSettings.ui_show_sidebar?.value === false) {
    return null;
  }
  const sidebarClasses = `sidebar ${isOpen ? "is-open" : ""} theme-${theme}`;
  const filteredSharingLinks = sharingLinks.filter(link => {
    if (link.id === 'shared') return renderableSettings.enableShared ?? true;
    if (link.id === 'player2') return renderableSettings.enablePlayer2 ?? true;
    if (link.id === 'player3') return renderableSettings.enablePlayer3 ?? true;
    if (link.id === 'player4') return renderableSettings.enablePlayer4 ?? true;
    return false;
  });
  const isWebrtc = streamMode === STREAM_MODE_WEBRTC;

  return (
    <>
      {isToggleVisible && (
        <div
          className='toggle-handle'
          onClick={toggleSidebar}
          title={`${isOpen ? 'Close' : 'Open'} Dashboard`}
        >
          <div className="toggle-indicator"></div>
        </div>
      )}
      {availablePlacements && (() => {
        const arrowBaseStyle = {
          position: 'absolute',
          width: '100px',
          height: '100px',
          backgroundColor: 'rgba(97, 218, 251, 0.8)',
          color: 'var(--sidebar-bg, #20232a)',
          border: '2px solid var(--sidebar-bg, #20232a)',
          borderRadius: '15px',
          fontSize: '48px',
          display: 'flex',
          justifyContent: 'center',
          alignItems: 'center',
          cursor: 'pointer',
          pointerEvents: 'all',
          boxShadow: '0 4px 15px rgba(0, 0, 0, 0.3)',
          transition: 'transform 0.2s ease',
        };

        const handleArrowClick = (e, direction, screen) => {
          e.stopPropagation();
          launchWindow(direction, screen);
        };

        return (
          <div 
            style={{
              position: 'fixed',
              top: 0,
              left: 0,
              width: '100vw',
              height: '100vh',
              zIndex: 9999,
              pointerEvents: 'auto'
            }}
            onClick={() => setAvailablePlacements(null)}
          >
            {availablePlacements.up && (
              <button style={{...arrowBaseStyle, top: '40px', left: '50%', transform: 'translateX(-50%)'}} onClick={(e) => handleArrowClick(e, 'up', availablePlacements.up)}>▲</button>
            )}
            {availablePlacements.down && (
              <button style={{...arrowBaseStyle, bottom: '40px', left: '50%', transform: 'translateX(-50%)'}} onClick={(e) => handleArrowClick(e, 'down', availablePlacements.down)}>▼</button>
            )}
            {availablePlacements.left && (
              <button style={{...arrowBaseStyle, left: '40px', top: '50%', transform: 'translateY(-50%)'}} onClick={(e) => handleArrowClick(e, 'left', availablePlacements.left)}>◄</button>
            )}
            {availablePlacements.right && (
              <button style={{...arrowBaseStyle, right: '40px', top: '50%', transform: 'translateY(-50%)'}} onClick={(e) => handleArrowClick(e, 'right', availablePlacements.right)}>►</button>
            )}
          </div>
        );
      })()}
      <div className={sidebarClasses}>
          <div className="sidebar-header">
            {uiShowLogo && (
              <a
                href="https://github.com/selkies-project/selkies"
                target="_blank"
                rel="noopener noreferrer"
              >
                <SelkiesLogo width={30} height={30} t={t} />
              </a>
            )}
            <a
              href="https://github.com/selkies-project/selkies"
              target="_blank"
              rel="noopener noreferrer"
            >
              <h2>{uiTitle}</h2>
            </a>
            <div className="header-controls">
            <div
              className={`theme-toggle ${theme}`}
              onClick={toggleTheme}
              title={t("toggleThemeTitle")}
            >
              <svg className="icon moon-icon" viewBox="0 0 24 24">
                <path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"></path>
              </svg>
              <svg className="icon sun-icon" viewBox="0 0 24 24">
                <circle cx="12" cy="12" r="5"></circle>
                <line x1="12" y1="1" x2="12" y2="3"></line>
                <line x1="12" y1="21" x2="12" y2="23"></line>
                <line x1="4.22" y1="4.22" x2="5.64" y2="5.64"></line>
                <line x1="18.36" y1="18.36" x2="19.78" y2="19.78"></line>
                <line x1="1" y1="12" x2="3" y2="12"></line>
                <line x1="21" y1="12" x2="23" y2="12"></line>
                <line x1="4.22" y1="19.78" x2="5.64" y2="18.36"></line>
                <line x1="18.36" y1="5.64" x2="19.78" y2="4.22"></line>
              </svg>
            </div>
            {(renderableSettings.fullscreen ?? true) && (
              <button
                className="header-action-button fullscreen-button"
                onClick={handleBrowserFullscreen}
                title={t("fullscreenTitle")}
              >
                <FullscreenIcon />
              </button>
            )}
            {(isMobile || hasDetectedTouch) ? (
              (renderableSettings.trackpad ?? true) && (
                <button
                  className={`header-action-button trackpad-mode-button ${isTrackpadModeActive ? "active" : ""}`}
                  onClick={handleToggleTrackpadMode}
                  title={t("trackpadModeTitle", "Trackpad Mode")}
                >
                  <TrackpadIcon />
                </button>
              )
            ) : (
              (renderableSettings.gamingMode ?? true) && (
                <button
                  className="header-action-button gaming-mode-button"
                  onClick={handleFullscreenRequest}
                  title={t("gamingModeTitle", "Gaming Mode")}
                >
                  <GamingModeIcon />
                </button>
              )
            )}
          </div>
        </div>

        {!isSecondaryDisplay && (renderableSettings.coreButtons ?? true) && (
          <div className="sidebar-action-buttons">
            {(renderableSettings.videoToggle ?? true) && (
              <button
                className={`action-button ${isVideoActive ? "active" : ""}`}
                onClick={handleVideoToggle}
                title={t(
                  isVideoActive
                    ? "buttons.videoStreamDisableTitle"
                    : "buttons.videoStreamEnableTitle"
                )}
              >
                <ScreenIcon />
              </button>
            )}
            {(renderableSettings.audioToggle ?? true) && (
              <button
                className={`action-button ${isAudioActive ? "active" : ""}`}
                onClick={handleAudioToggle}
                title={t(
                  isAudioActive
                    ? "buttons.audioStreamDisableTitle"
                    : "buttons.audioStreamEnableTitle"
                )}
              >
                <SpeakerIcon />
              </button>
            )}
            {(renderableSettings.microphoneToggle ?? true) && (
              <button
                className={`action-button ${isMicrophoneActive ? "active" : ""}`}
                onClick={handleMicrophoneToggle}
                title={t(
                  isMicrophoneActive
                    ? "buttons.microphoneDisableTitle"
                    : "buttons.microphoneEnableTitle"
                )}
              >
                <MicrophoneIcon />
              </button>
            )}
            {(renderableSettings.gamepadToggle ?? true) && (
              <button
                className={`action-button ${isGamepadEnabled ? "active" : ""}`}
                onClick={handleGamepadToggle}
                title={t(
                  isGamepadEnabled
                    ? "buttons.gamepadDisableTitle"
                    : "buttons.gamepadEnableTitle"
                )}
              >
                <GamepadIcon />
              </button>
            )}
          </div>
        )}
        
        {(isMobile || hasDetectedTouch) && (renderableSettings.softButtons ?? true) && (
          <>
            <div className="sidebar-section-divider"></div>
            <div className="sidebar-mobile-key-actions">
              <button
                className={`mobile-key-button ${heldKeys.Control ? "active" : ""}`}
                onClick={() => handleHoldKeyClick('Control', 'ControlLeft')}
                onMouseDown={(e) => e.preventDefault()}
              >
                CTL
              </button>
              <button
                className={`mobile-key-button ${heldKeys.Alt ? "active" : ""}`}
                onClick={() => handleHoldKeyClick('Alt', 'AltLeft')}
                onMouseDown={(e) => e.preventDefault()}
              >
                ALT
              </button>
              <button
                className={`mobile-key-button ${heldKeys.Meta ? "active" : ""}`}
                onClick={() => handleHoldKeyClick('Meta', 'MetaLeft')}
                onMouseDown={(e) => e.preventDefault()}
              >
                WIN
              </button>
              <button
                className="mobile-key-button"
                onClick={() => handleOnceKeyClick('Tab', 'Tab')}
                onMouseDown={(e) => e.preventDefault()}
              >
                TAB
              </button>
              <button
                className="mobile-key-button"
                onClick={() => handleOnceKeyClick('Escape', 'Escape')}
                onMouseDown={(e) => e.preventDefault()}
              >
                ESC
              </button>
              <button
                className={`mobile-key-button icon-button ${isKeyboardButtonVisible ? "active" : ""}`}
                onClick={toggleKeyboardButtonVisibility}
              >
                <KeyboardIcon />
              </button>
            </div>
          </>
        )}

        {(renderableSettings.videoSettings ?? true) && (
          <div className="sidebar-section">
            <div
              className="sidebar-section-header"
              onClick={() => toggleSection("settings")}
              role="button"
              aria-expanded={sectionsOpen.settings}
              aria-controls="settings-content"
              tabIndex="0"
              onKeyDown={(e) =>
                (e.key === "Enter" || e.key === " ") && toggleSection("settings")
              }
            >
              <h3>{t("sections.video.title")}</h3>
              <span className="section-toggle-icon">
                {sectionsOpen.settings ? <CaretUpIcon /> : <CaretDownIcon />}
              </span>
            </div>
            {sectionsOpen.settings && (
                // TODO: Change of Stream mode is configurable from server side?
                <div className="sidebar-section-content" id="settings-content">
                  {(renderableSettings.enableDualMode ?? false) && (
                    <div className="dev-setting-item">
                      {" "}
                      <label htmlFor="streamModeSelect">
                        {t("streamingModeTitle", "Streaming Mode")}
                      </label>{" "}
                      <select
                        id="streamModeSelect"
                        value={streamMode}
                        onChange={handleStreamModeChange}
                      >
                        {" "}
                        {STREAMING_MODES.map((mode) => (
                          <option key={mode} value={mode}>
                            {mode}
                          </option>
                        ))}{" "}
                      </select>{" "}
                    </div>
                  )}
                {(renderableSettings.encoder ?? true) && (
                  <div className="dev-setting-item">
                    <label htmlFor="encoderSelect">
                      {t("sections.video.encoderLabel")}
                    </label>
                    <select
                      id="encoderSelect"
                      value={isWebrtc? encoderRTC: encoder}
                      onChange={handleEncoderChange}
                      disabled={!serverSettings || serverSettings.encoder?.allowed?.length <= 1}
                    >
                      {(serverSettings?.encoder?.allowed || dynamicEncoderOptions).map((enc) => (
                        <option key={enc} value={enc}>
                          {enc}
                        </option>
                      ))}
                    </select>
                  </div>
                )}
                {showFPS && (renderableSettings.framerate ?? true) && (
                  <div className="dev-setting-item">
                    <label htmlFor="framerateSlider">
                      {t("sections.video.framerateLabel", {
                        framerate: framerate,
                      })}
                    </label>
                    <input
                      type="range"
                      id="framerateSlider"
                      min={serverSettings?.framerate?.min || 8}
                      max={serverSettings?.framerate?.max || 165}
                      step="1"
                      value={framerate}
                      onChange={handleFramerateChange}
                      disabled={!serverSettings || serverSettings.framerate?.min === serverSettings.framerate?.max}
                    />
                  </div>
                )}
                {isWebrtc && (renderableSettings.video_bitrate ?? true) && (
                  <div className="dev-setting-item">
                    <label htmlFor="videoBitrateSlider">
                      {t("sections.video.bitrateLabel", {
                        bitrate: videoBitrate,
                      })}
                    </label>
                    <input
                      type="range"
                      id="videoBitrateSlider"
                      min={serverSettings?.video_bitrate?.min || 1}
                      max={serverSettings?.video_bitrate?.max || 100}
                      step="1"
                      value={videoBitrate}
                      onChange={handleVideoBitrateChange}
                      disabled={!serverSettings || serverSettings.video_bitrate?.min === serverSettings.video_bitrate?.max}
                    />
                  </div>  
                )}
                {!isWebrtc && showJpegOptions && (
                  <>
                    {(renderableSettings.jpeg_quality ?? true) && (
                      <div className="dev-setting-item">
                        <label htmlFor="jpegQualitySlider">
                          {t("sections.video.jpegQualityLabel", {
                            jpegQuality: jpeg_quality,
                          })}
                        </label>
                        <input
                          type="range"
                          id="jpegQualitySlider"
                          min={serverSettings?.jpeg_quality?.min || 1}
                          max={serverSettings?.jpeg_quality?.max || 100}
                          step="1"
                          value={jpeg_quality}
                          onChange={handleJpegQualityChange}
                          disabled={!serverSettings || serverSettings.jpeg_quality?.min === serverSettings.jpeg_quality?.max}
                        />
                      </div>
                    )}
                    {(renderableSettings.paint_over_jpeg_quality ?? true) && (
                      <div className="dev-setting-item">
                        <label htmlFor="paintOverJpegQualitySlider">
                          {t("sections.video.paintOverJpegQualityLabel", {
                            paintOverJpegQuality: paint_over_jpeg_quality,
                          })}
                        </label>
                        <input
                          type="range"
                          id="paintOverJpegQualitySlider"
                          min={serverSettings?.paint_over_jpeg_quality?.min || 1}
                          max={serverSettings?.paint_over_jpeg_quality?.max || 100}
                          step="1"
                          value={paint_over_jpeg_quality}
                          onChange={handlePaintOverJpegQualityChange}
                          disabled={!serverSettings || serverSettings.paint_over_jpeg_quality?.min === serverSettings.paint_over_jpeg_quality?.max}
                        />
                      </div>
                    )}
                  </>
                )}
                {!isWebrtc && showCRF && (renderableSettings.h264_crf ?? true) && (
                  <div className="dev-setting-item">
                    <label htmlFor="videoCRFSlider">
                      {t("sections.video.crfLabel", { crf: h264_crf })}
                    </label>
                    <input
                      type="range"
                      id="videoCRFSlider"
                      min={serverSettings?.h264_crf?.min || 5}
                      max={serverSettings?.h264_crf?.max || 50}
                      step="1"
                      value={h264_crf}
                      onChange={handleVideoCRFChange}
                      disabled={!serverSettings || serverSettings.h264_crf?.min === serverSettings.h264_crf?.max}
                      style={{ direction: 'rtl' }}
                    />
                  </div>
                )}
                {!isWebrtc && showCRF && (renderableSettings.h264PaintoverCRF ?? true) && (
                  <div className="dev-setting-item">
                    <label htmlFor="h264PaintoverCRFSlider">
                      {t("sections.video.paintoverCrfLabel", { crf: h264PaintoverCRF })}
                    </label>
                    <input
                      type="range"
                      id="h264PaintoverCRFSlider"
                      min={serverSettings?.h264_paintover_crf?.min || 5}
                      max={serverSettings?.h264_paintover_crf?.max || 50}
                      step="1"
                      value={h264PaintoverCRF}
                      onChange={handleH264PaintoverCRFChange}
                      disabled={!serverSettings || serverSettings.h264_paintover_crf?.min === serverSettings.h264_paintover_crf?.max}
                      style={{ direction: 'rtl' }}
                    />
                  </div>
                )}
                {!isWebrtc && showPaintOverQualityToggle && (renderableSettings.usePaintOverQuality ?? true) && (
                  <div className="dev-setting-item toggle-item">
                    <label htmlFor="usePaintOverQualityToggle">
                      {t("sections.video.usePaintOverQualityLabel", "Use Paint-Over Quality")}
                    </label>
                    <button
                      id="usePaintOverQualityToggle"
                      className={`toggle-button-sidebar ${usePaintOverQuality ? "active" : ""}`}
                      onClick={handleUsePaintOverQualityToggle}
                      aria-pressed={usePaintOverQuality}
                      disabled={!serverSettings || serverSettings.use_paint_over_quality?.locked}
                      title={t(usePaintOverQuality ? "buttons.usePaintOverQualityDisableTitle" : "buttons.usePaintOverQualityEnableTitle")}
                    >
                      <span className="toggle-button-sidebar-knob"></span>
                    </button>
                  </div>
                )}
                {!isWebrtc && showH264Options && (renderableSettings.h264StreamingMode ?? true) && (
                  <div className="dev-setting-item toggle-item">
                    <label 
                      htmlFor="h264StreamingModeToggle"
                      title={t("sections.video.streamingModeDetails")}
                    >
                      {t("sections.video.streamingModeLabel", "Turbo")}
                    </label>
                    <button
                      id="h264StreamingModeToggle"
                      className={`toggle-button-sidebar ${h264StreamingMode ? "active" : ""}`}
                      onClick={handleH264StreamingModeToggle}
                      aria-pressed={h264StreamingMode}
                      disabled={!serverSettings || serverSettings.h264_streaming_mode?.locked}
                      title={t(h264StreamingMode ? "buttons.h264StreamingModeDisableTitle" : "buttons.h264StreamingModeEnableTitle")}
                    >
                      <span className="toggle-button-sidebar-knob"></span>
                    </button>
                  </div>
                )}
                {!isWebrtc && showH264Options && (renderableSettings.h264FullColor ?? true) && (
                  <div className="dev-setting-item toggle-item">
                    <label htmlFor="h264FullColorToggle">
                      {t("sections.video.fullColorLabel")}
                    </label>
                    <button
                      id="h264FullColorToggle"
                      className={`toggle-button-sidebar ${h264FullColor ? "active" : ""}`}
                      onClick={handleH264FullColorToggle}
                      aria-pressed={h264FullColor}
                      disabled={!serverSettings || serverSettings.h264_fullcolor?.locked}
                      title={t(h264FullColor ? "buttons.h264FullColorDisableTitle" : "buttons.h264FullColorEnableTitle")}
                    >
                      <span className="toggle-button-sidebar-knob"></span>
                    </button>
                  </div>
                )}
                {!isWebrtc && showH264Options && (renderableSettings.use_cpu ?? true) && (
                  <div className="dev-setting-item toggle-item">
                    <label htmlFor="useCpuToggle">
                      {t("sections.video.useCpuLabel", "CPU Encoding")}
                    </label>
                    <button
                      id="useCpuToggle"
                      className={`toggle-button-sidebar ${use_cpu ? "active" : ""}`}
                      onClick={handleUseCpuToggle}
                      aria-pressed={use_cpu}
                      disabled={!serverSettings || serverSettings.use_cpu?.locked}
                      title={t(use_cpu ? "buttons.useCpuDisableTitle" : "buttons.useCpuEnableTitle")}
                    >
                      <span className="toggle-button-sidebar-knob"></span>
                    </button>
                  </div>
                )}
              </div>
            )}
          </div>
        )}

        {(renderableSettings.screenSettings ?? true) && (
          <div className="sidebar-section">
            <div
              className="sidebar-section-header"
              onClick={() => toggleSection("screenSettings")}
              role="button"
              aria-expanded={sectionsOpen.screenSettings}
              aria-controls="screen-settings-content"
              tabIndex="0"
              onKeyDown={(e) =>
                (e.key === "Enter" || e.key === " ") &&
                toggleSection("screenSettings")
              }
            >
              <h3>{t("sections.screen.title")}</h3>
              <span className="section-toggle-icon">
                {sectionsOpen.screenSettings ? (
                  <CaretUpIcon />
                ) : (
                  <CaretDownIcon />
                )}
              </span>
            </div>
            {sectionsOpen.screenSettings && (
              <div
                className="sidebar-section-content"
                id="screen-settings-content"
              >
                {!isSecondaryDisplay && (
                  <>
                    {serverSettings?.second_screen?.value && (
                      <button
                        className="resolution-button toggle-button"
                        onClick={handleAddScreenClick}
                        style={{ marginBottom: "10px" }}
                        title={t("sections.screen.addScreenTitle", "Add a second screen")}
                      >
                        {t("sections.screen.addScreenButton", "Add Screen +")}
                      </button>
                    )}
                    {(renderableSettings.hidpi ?? true) && (
                      <div className="dev-setting-item toggle-item">
                        <label htmlFor="hidpiToggle">
                          {t("sections.screen.hidpiLabel", "HiDPI (Pixel Perfect)")}
                        </label>
                        <button
                          id="hidpiToggle"
                          className={`toggle-button-sidebar ${hidpiEnabled ? "active" : ""}`}
                          onClick={handleHidpiToggle}
                          aria-pressed={hidpiEnabled}
                          title={t(hidpiEnabled ? "sections.screen.hidpiDisableTitle" : "sections.screen.hidpiEnableTitle",
                                  hidpiEnabled ? "Disable HiDPI (Use CSS Scaling)" : "Enable HiDPI (Pixel Perfect)")}
                        >
                          <span className="toggle-button-sidebar-knob"></span>
                        </button>
                      </div>
                    )}
                    <div className="dev-setting-item toggle-item">
                      <label htmlFor="antiAliasingToggle">
                        {t("sections.screen.antiAliasingLabel", "Anti-aliasing")}
                      </label>
                      <button
                        id="antiAliasingToggle"
                        className={`toggle-button-sidebar ${antiAliasing ? "active" : ""}`}
                        onClick={handleAntiAliasingToggle}
                        aria-pressed={antiAliasing}
                        title={t(antiAliasing ? "sections.screen.antiAliasingDisableTitle" : "sections.screen.antiAliasingEnableTitle",
                                  antiAliasing ? "Disable anti-aliasing (force pixelated)" : "Enable anti-aliasing (smooth on scaling)")}
                      >
                        <span className="toggle-button-sidebar-knob"></span>
                      </button>
                    </div>
                    {(renderableSettings.use_browser_cursors ?? true) && (
                      <div className="dev-setting-item toggle-item">
                        <label htmlFor="useBrowserCursorsToggle">
                          {t("sections.screen.useNativeCursorStylesLabel", "Use CSS cursors")}
                        </label>
                        <button
                          id="useBrowserCursorsToggle"
                          className={`toggle-button-sidebar ${use_browser_cursors ? "active" : ""}`}
                          onClick={handleUseBrowserCursorsToggle}
                          aria-pressed={use_browser_cursors}
                          title={t(use_browser_cursors ? "sections.screen.useNativeCursorStylesDisableTitle" : "sections.screen.useNativeCursorStylesEnableTitle",
                                  use_browser_cursors ? "Use canvas cursor rendering (Paint to canvas)" : "Use CSS cursor rendering (Replace system cursors)")}
                        >
                          <span className="toggle-button-sidebar-knob"></span>
                        </button>
                      </div>
                    )}
                    {(renderableSettings.uiScaling ?? true) && (
                      <div className="dev-setting-item">
                        <label htmlFor="uiScalingSelect">
                          {t("sections.screen.uiScalingLabel", "UI Scaling")}
                        </label>
                        <select
                          id="uiScalingSelect"
                          value={selectedDpi}
                          onChange={handleDpiScalingChange}
                          disabled={!serverSettings || serverSettings.scaling_dpi?.allowed?.length <= 1}
                        >
                          {(serverSettings?.scaling_dpi?.allowed || []).map((dpiValue) => {
                            const percent = Math.round((parseInt(dpiValue, 10) / 96) * 100);
                            const label = `${percent}%`;
                            return (
                              <option key={dpiValue} value={dpiValue}>
                                {dpiValue === String(currentDeviceDpi) ? `${label} *` : label}
                              </option>
                            );
                          })}
                        </select>
                      </div>
                    )}
                  </>
                )}
                {(!serverSettings?.is_manual_resolution_mode?.locked) && (
                  <>
                    <div className="dev-setting-item">
                      <label htmlFor="resolutionPresetSelect">
                        {t("sections.screen.presetLabel")}
                      </label>
                      <select
                        id="resolutionPresetSelect"
                        value={presetValue}
                        onChange={handlePresetChange}
                      >
                        {translatedCommonResolutions.map((res, i) => (
                          <option key={i} value={res.value} disabled={i === 0}>
                            {res.text}
                          </option>
                        ))}
                      </select>
                    </div>
                    <div className="resolution-manual-inputs">
                      <div className="dev-setting-item manual-input-item">
                        <label htmlFor="manualWidthInput">
                          {t("sections.screen.widthLabel")}
                        </label>
                        <input
                          className="allow-native-input"
                          type="number"
                          id="manualWidthInput"
                          min="1"
                          step="2"
                          placeholder={t("sections.screen.widthPlaceholder")}
                          value={manual_width}
                          onChange={handleManualWidthChange}
                        />
                      </div>
                      <div className="dev-setting-item manual-input-item">
                        <label htmlFor="manualHeightInput">
                          {t("sections.screen.heightLabel")}
                        </label>
                        <input
                          className="allow-native-input"
                          type="number"
                          id="manualHeightInput"
                          min="1"
                          step="2"
                          placeholder={t("sections.screen.heightPlaceholder")}
                          value={manual_height}
                          onChange={handleManualHeightChange}
                        />
                      </div>
                    </div>
                    <div className="resolution-action-buttons">
                      <button
                        className="resolution-button"
                        onClick={handleSetManualResolution}
                      >
                        {t("sections.screen.setManualButton")}
                      </button>
                      <button
                        className="resolution-button reset-button"
                        onClick={handleResetResolution}
                      >
                        {t("sections.screen.resetButton")}
                      </button>
                    </div>
                  </>
                )}
                <button
                  className={`resolution-button toggle-button ${
                    scaleLocally ? "active" : ""
                  }`}
                  onClick={handleScaleLocallyToggle}
                  style={{ marginTop: "10px" }}
                  title={t(
                    scaleLocally
                      ? "sections.screen.scaleLocallyTitleDisable"
                      : "sections.screen.scaleLocallyTitleEnable"
                  )}
                >
                  {t("sections.screen.scaleLocallyLabel")}
                  {t(
                    scaleLocally
                      ? "sections.screen.scaleLocallyOn"
                      : "sections.screen.scaleLocallyOff"
                  )}
                </button>
              </div>
            )}
          </div>
        )}

        {!isSecondaryDisplay && (
          <>
            {(renderableSettings.audioSettings ?? true) && (
              <div className="sidebar-section">
                <div
                  className="sidebar-section-header"
                  onClick={() => toggleSection("audioSettings")}
                  role="button"
                  aria-expanded={sectionsOpen.audioSettings}
                  aria-controls="audio-settings-content"
                  tabIndex="0"
                  onKeyDown={(e) => (e.key === "Enter" || e.key === " ") &&
                    toggleSection("audioSettings")}
                >
                  <h3>{t("sections.audio.title")}</h3>
                  <span className="section-toggle-icon">
                    {isLoadingAudioDevices ? (
                      <SpinnerIcon />
                    ) : sectionsOpen.audioSettings ? (
                      <CaretUpIcon />
                    ) : (
                      <CaretDownIcon />
                    )}
                  </span>
                </div>
                {sectionsOpen.audioSettings && (
                  <div
                    className="sidebar-section-content"
                    id="audio-settings-content"
                  >
                    {audioDeviceError && (
                      <div className="error-message">{audioDeviceError}</div>
                    )}
                    <div className="dev-setting-item">
                      <label htmlFor="audioInputSelect">
                        {t("sections.audio.inputLabel")}
                      </label>
                      <select
                        id="audioInputSelect"
                        value={selectedInputDeviceId}
                        onChange={handleAudioInputChange}
                        disabled={isLoadingAudioDevices || !!audioDeviceError}
                        className="audio-device-select"
                      >
                        {audioInputDevices.map((d) => (
                          <option key={d.deviceId} value={d.deviceId}>
                            {d.label}
                          </option>
                        ))}
                      </select>
                    </div>
                    {isOutputSelectionSupported && (
                      <div className="dev-setting-item">
                        <label htmlFor="audioOutputSelect">
                          {t("sections.audio.outputLabel")}
                        </label>
                        <select
                          id="audioOutputSelect"
                          value={selectedOutputDeviceId}
                          onChange={handleAudioOutputChange}
                          disabled={isLoadingAudioDevices || !!audioDeviceError}
                          className="audio-device-select"
                        >
                          {audioOutputDevices.map((d) => (
                            <option key={d.deviceId} value={d.deviceId}>
                              {d.label}
                            </option>
                          ))}
                        </select>
                      </div>
                    )}
                    {isWebrtc && isOutputSelectionSupported && renderableSettings.audio_bitrate && (
                      <div className="dev-setting-item">
                        <label htmlFor="audioBitrateSlider">
                          {t("sections.audio.bitrateLabel", {
                            bitrate: audioBitrate/ 1000,
                          })}
                        </label>
                        <input
                          type="range"
                          id="audioBitrateSlider"
                          min={serverSettings?.audio_bitrate?.allowed?.[0] || 64000}
                          max={serverSettings?.audio_bitrate?.allowed?.[serverSettings.audio_bitrate.allowed.length - 1] || 320000}
                          step="64000"
                          value={audioBitrate}
                          onChange={handleAudioBitrateChange}
                          disabled={!serverSettings || serverSettings.audio_bitrate?.allowed?.length <= 1}
                        />
                      </div>
                    )}
                    {!isOutputSelectionSupported &&
                      !isLoadingAudioDevices &&
                      !audioDeviceError && (
                        <p className="device-support-notice">
                          {t("sections.audio.outputNotSupported")}
                        </p>
                      )}
                  </div>
                )}
              </div>
            )}
            {(renderableSettings.stats ?? true) && (
              <div className="sidebar-section">
                <div
                  className="sidebar-section-header"
                  onClick={() => toggleSection("stats")}
                  role="button"
                  aria-expanded={sectionsOpen.stats}
                  aria-controls="stats-content"
                  tabIndex="0"
                  onKeyDown={(e) => (e.key === "Enter" || e.key === " ") && toggleSection("stats")}
                >
                  <h3>{t("sections.stats.title")}</h3>
                  <span className="section-toggle-icon">
                    {sectionsOpen.stats ? <CaretUpIcon /> : <CaretDownIcon />}
                  </span>
                </div>
                {sectionsOpen.stats && (
                  <div className="sidebar-section-content" id="stats-content">
                    <div className="stats-gauges">
                      <div
                        className="gauge-container"
                        onMouseEnter={(e) => handleMouseEnter(e, "cpu")}
                        onMouseLeave={handleMouseLeave}
                      >
                        <svg
                          width={gaugeSize}
                          height={gaugeSize}
                          viewBox={`0 0 ${gaugeSize} ${gaugeSize}`}
                        >
                          <circle
                            stroke="var(--item-border)"
                            fill="transparent"
                            strokeWidth={gaugeStrokeWidth}
                            r={gaugeRadius}
                            cx={gaugeCenter}
                            cy={gaugeCenter} />
                          <circle
                            stroke="var(--sidebar-header-color)"
                            fill="transparent"
                            strokeWidth={gaugeStrokeWidth}
                            r={gaugeRadius}
                            cx={gaugeCenter}
                            cy={gaugeCenter}
                            transform={`rotate(-90 ${gaugeCenter} ${gaugeCenter})`}
                            style={{
                              strokeDasharray: gaugeCircumference,
                              strokeDashoffset: cpuOffset,
                              transition: "stroke-dashoffset 0.3s ease-in-out",
                              strokeLinecap: "round",
                            }} />
                          <text
                            x={gaugeCenter}
                            y={gaugeCenter}
                            textAnchor="middle"
                            dominantBaseline="central"
                            fontSize={`${gaugeSize / 5}px`}
                            fill="var(--sidebar-text)"
                            fontWeight="bold"
                          >
                            {Math.round(
                              Math.max(0, Math.min(100, cpuPercent || 0))
                            )}%
                          </text>
                        </svg>
                        <div className="gauge-label">
                          {t("sections.stats.cpuLabel")}
                        </div>
                      </div>
                      <div
                        className="gauge-container"
                        onMouseEnter={(e) => handleMouseEnter(e, "sysmem")}
                        onMouseLeave={handleMouseLeave}
                      >
                        <svg
                          width={gaugeSize}
                          height={gaugeSize}
                          viewBox={`0 0 ${gaugeSize} ${gaugeSize}`}
                        >
                          <circle
                            stroke="var(--item-border)"
                            fill="transparent"
                            strokeWidth={gaugeStrokeWidth}
                            r={gaugeRadius}
                            cx={gaugeCenter}
                            cy={gaugeCenter} />
                          <circle
                            stroke="var(--sidebar-header-color)"
                            fill="transparent"
                            strokeWidth={gaugeStrokeWidth}
                            r={gaugeRadius}
                            cx={gaugeCenter}
                            cy={gaugeCenter}
                            transform={`rotate(-90 ${gaugeCenter} ${gaugeCenter})`}
                            style={{
                              strokeDasharray: gaugeCircumference,
                              strokeDashoffset: sysMemOffset,
                              transition: "stroke-dashoffset 0.3s ease-in-out",
                              strokeLinecap: "round",
                            }} />
                          <text
                            x={gaugeCenter}
                            y={gaugeCenter}
                            textAnchor="middle"
                            dominantBaseline="central"
                            fontSize={`${gaugeSize / 5}px`}
                            fill="var(--sidebar-text)"
                            fontWeight="bold"
                          >
                            {Math.round(
                              Math.max(0, Math.min(100, sysMemPercent || 0))
                            )}
                            %
                          </text>
                        </svg>
                        <div className="gauge-label">
                          {t("sections.stats.sysMemLabel")}
                        </div>
                      </div>
                      {window.gpu_stats && (
                        <>
                          <div
                            className="gauge-container"
                            onMouseEnter={(e) => handleMouseEnter(e, "gpu")}
                            onMouseLeave={handleMouseLeave}
                          >
                            <svg
                              width={gaugeSize}
                              height={gaugeSize}
                              viewBox={`0 0 ${gaugeSize} ${gaugeSize}`}
                            >
                              <circle
                                stroke="var(--item-border)"
                                fill="transparent"
                                strokeWidth={gaugeStrokeWidth}
                                r={gaugeRadius}
                                cx={gaugeCenter}
                                cy={gaugeCenter} />
                              <circle
                                stroke="var(--sidebar-header-color)"
                                fill="transparent"
                                strokeWidth={gaugeStrokeWidth}
                                r={gaugeRadius}
                                cx={gaugeCenter}
                                cy={gaugeCenter}
                                transform={`rotate(-90 ${gaugeCenter} ${gaugeCenter})`}
                                style={{
                                  strokeDasharray: gaugeCircumference,
                                  strokeDashoffset: gpuOffset,
                                  transition: "stroke-dashoffset 0.3s ease-in-out",
                                  strokeLinecap: "round",
                                }} />
                              <text
                                x={gaugeCenter}
                                y={gaugeCenter}
                                textAnchor="middle"
                                dominantBaseline="central"
                                fontSize={`${gaugeSize / 5}px`}
                                fill="var(--sidebar-text)"
                                fontWeight="bold"
                              >
                                {Math.round(
                                  Math.max(0, Math.min(100, gpuPercent || 0))
                                )}%
                              </text>
                            </svg>
                            <div className="gauge-label">
                              {t("sections.stats.gpuLabel")}
                            </div>
                          </div>
                          <div
                            className="gauge-container"
                            onMouseEnter={(e) => handleMouseEnter(e, "gpumem")}
                            onMouseLeave={handleMouseLeave}
                          >
                            <svg
                              width={gaugeSize}
                              height={gaugeSize}
                              viewBox={`0 0 ${gaugeSize} ${gaugeSize}`}
                            >
                              <circle
                                stroke="var(--item-border)"
                                fill="transparent"
                                strokeWidth={gaugeStrokeWidth}
                                r={gaugeRadius}
                                cx={gaugeCenter}
                                cy={gaugeCenter} />
                              <circle
                                stroke="var(--sidebar-header-color)"
                                fill="transparent"
                                strokeWidth={gaugeStrokeWidth}
                                r={gaugeRadius}
                                cx={gaugeCenter}
                                cy={gaugeCenter}
                                transform={`rotate(-90 ${gaugeCenter} ${gaugeCenter})`}
                                style={{
                                  strokeDasharray: gaugeCircumference,
                                  strokeDashoffset: gpuMemOffset,
                                  transition: "stroke-dashoffset 0.3s ease-in-out",
                                  strokeLinecap: "round",
                                }} />
                              <text
                                x={gaugeCenter}
                                y={gaugeCenter}
                                textAnchor="middle"
                                dominantBaseline="central"
                                fontSize={`${gaugeSize / 5}px`}
                                fill="var(--sidebar-text)"
                                fontWeight="bold"
                              >
                                {Math.round(
                                  Math.max(0, Math.min(100, gpuMemPercent || 0))
                                )}
                                %
                              </text>
                            </svg>
                            <div className="gauge-label">
                              {t("sections.stats.gpuMemLabel")}
                            </div>
                          </div>
                        </>
                      )}
                      <div
                        className="gauge-container"
                        onMouseEnter={(e) => handleMouseEnter(e, "fps")}
                        onMouseLeave={handleMouseLeave}
                      >
                        <svg
                          width={gaugeSize}
                          height={gaugeSize}
                          viewBox={`0 0 ${gaugeSize} ${gaugeSize}`}
                        >
                          <circle
                            stroke="var(--item-border)"
                            fill="transparent"
                            strokeWidth={gaugeStrokeWidth}
                            r={gaugeRadius}
                            cx={gaugeCenter}
                            cy={gaugeCenter} />
                          <circle
                            stroke="var(--sidebar-header-color)"
                            fill="transparent"
                            strokeWidth={gaugeStrokeWidth}
                            r={gaugeRadius}
                            cx={gaugeCenter}
                            cy={gaugeCenter}
                            transform={`rotate(-90 ${gaugeCenter} ${gaugeCenter})`}
                            style={{
                              strokeDasharray: gaugeCircumference,
                              strokeDashoffset: fpsOffset,
                              transition: "stroke-dashoffset 0.3s ease-in-out",
                              strokeLinecap: "round",
                            }} />
                          <text
                            x={gaugeCenter}
                            y={gaugeCenter}
                            textAnchor="middle"
                            dominantBaseline="central"
                            fontSize={`${gaugeSize / 5}px`}
                            fill="var(--sidebar-text)"
                            fontWeight="bold"
                          >
                            {clientFps}
                          </text>
                        </svg>
                        <div className="gauge-label">
                          {t("sections.stats.fpsLabel")}
                        </div>
                      </div>
                      <div
                        className="gauge-container"
                        onMouseEnter={(e) => handleMouseEnter(e, "audio")}
                        onMouseLeave={handleMouseLeave}
                      >
                        <svg
                          width={gaugeSize}
                          height={gaugeSize}
                          viewBox={`0 0 ${gaugeSize} ${gaugeSize}`}
                        >
                          <circle
                            stroke="var(--item-border)"
                            fill="transparent"
                            strokeWidth={gaugeStrokeWidth}
                            r={gaugeRadius}
                            cx={gaugeCenter}
                            cy={gaugeCenter} />
                          <circle
                            stroke="var(--sidebar-header-color)"
                            fill="transparent"
                            strokeWidth={gaugeStrokeWidth}
                            r={gaugeRadius}
                            cx={gaugeCenter}
                            cy={gaugeCenter}
                            transform={`rotate(-90 ${gaugeCenter} ${gaugeCenter})`}
                            style={{
                              strokeDasharray: gaugeCircumference,
                              strokeDashoffset: audioBufferOffset,
                              transition: "stroke-dashoffset 0.3s ease-in-out",
                              strokeLinecap: "round",
                            }} />
                          <text
                            x={gaugeCenter}
                            y={gaugeCenter}
                            textAnchor="middle"
                            dominantBaseline="central"
                            fontSize={`${gaugeSize / 5}px`}
                            fill="var(--sidebar-text)"
                            fontWeight="bold"
                          >
                            {audioBuffer}
                          </text>
                        </svg>
                        <div className="gauge-label">
                          {t("sections.stats.audioLabel")}
                        </div>
                      </div>
                      <div
                        className="gauge-container"
                        onMouseEnter={(e) => handleMouseEnter(e, "bandwidth")}
                        onMouseLeave={handleMouseLeave}
                      >
                        <svg
                          width={gaugeSize}
                          height={gaugeSize}
                          viewBox={`0 0 ${gaugeSize} ${gaugeSize}`}
                        >
                          <circle
                            stroke="var(--item-border)"
                            fill="transparent"
                            strokeWidth={gaugeStrokeWidth}
                            r={gaugeRadius}
                            cx={gaugeCenter}
                            cy={gaugeCenter} />
                          <circle
                            stroke="var(--sidebar-header-color)"
                            fill="transparent"
                            strokeWidth={gaugeStrokeWidth}
                            r={gaugeRadius}
                            cx={gaugeCenter}
                            cy={gaugeCenter}
                            transform={`rotate(-90 ${gaugeCenter} ${gaugeCenter})`}
                            style={{
                              strokeDasharray: gaugeCircumference,
                              strokeDashoffset: bandwidthOffset,
                              transition: "stroke-dashoffset 0.3s ease-in-out",
                              strokeLinecap: "round",
                            }} />
                          <text
                            x={gaugeCenter}
                            y={gaugeCenter}
                            textAnchor="middle"
                            dominantBaseline="central"
                            fontSize={`${gaugeSize / 5}px`}
                            fill="var(--sidebar-text)"
                            fontWeight="bold"
                          >
                            {Math.round(bandwidthMbps)}
                          </text>
                        </svg>
                        <div className="gauge-label">
                          {t("sections.stats.bandwidthLabel", "Bandwidth")}
                        </div>
                      </div>
                      <div
                        className="gauge-container"
                        onMouseEnter={(e) => handleMouseEnter(e, "latency")}
                        onMouseLeave={handleMouseLeave}
                      >
                        <svg
                          width={gaugeSize}
                          height={gaugeSize}
                          viewBox={`0 0 ${gaugeSize} ${gaugeSize}`}
                        >
                          <circle
                            stroke="var(--item-border)"
                            fill="transparent"
                            strokeWidth={gaugeStrokeWidth}
                            r={gaugeRadius}
                            cx={gaugeCenter}
                            cy={gaugeCenter} />
                          <circle
                            stroke="var(--sidebar-header-color)"
                            fill="transparent"
                            strokeWidth={gaugeStrokeWidth}
                            r={gaugeRadius}
                            cx={gaugeCenter}
                            cy={gaugeCenter}
                            transform={`rotate(-90 ${gaugeCenter} ${gaugeCenter})`}
                            style={{
                              strokeDasharray: gaugeCircumference,
                              strokeDashoffset: latencyOffset,
                              transition: "stroke-dashoffset 0.3s ease-in-out",
                              strokeLinecap: "round",
                            }} />
                          <text
                            x={gaugeCenter}
                            y={gaugeCenter}
                            textAnchor="middle"
                            dominantBaseline="central"
                            fontSize={`${gaugeSize / 5}px`}
                            fill="var(--sidebar-text)"
                            fontWeight="bold"
                          >
                            {Math.round(latencyMs)}
                          </text>
                        </svg>
                        <div className="gauge-label">
                          {t("sections.stats.latencyLabel", "Latency")}
                        </div>
                      </div>
                    </div>
                  </div>
                )}
              </div>
            )}

            {(renderableSettings.clipboard ?? true) && (
              <div className="sidebar-section">
                <div
                  className="sidebar-section-header"
                  onClick={() => toggleSection("clipboard")}
                  role="button"
                  aria-expanded={sectionsOpen.clipboard}
                  aria-controls="clipboard-content"
                  tabIndex="0"
                  onKeyDown={(e) =>
                    (e.key === "Enter" || e.key === " ") && toggleSection("clipboard")
                  }
                >
                  <h3>{t("sections.clipboard.title")}</h3>
                  <span className="section-toggle-icon">
                    {sectionsOpen.clipboard ? <CaretUpIcon /> : <CaretDownIcon />}
                  </span>
                </div>
                {sectionsOpen.clipboard && (
                  <div className="sidebar-section-content" id="clipboard-content">
                    {(renderableSettings.binaryClipboard ?? true) && (
                      <div className="dev-setting-item toggle-item">
                        <label 
                          htmlFor="enableBinaryClipboardToggle"
                          title={t("sections.clipboard.binaryModeDetails")}
                        >
                          {t("sections.clipboard.binaryModeLabel", "Image Support")}
                        </label>
                        <button
                          id="enableBinaryClipboardToggle"
                          className={`toggle-button-sidebar ${enableBinaryClipboard ? "active" : ""}`}
                          onClick={handleEnableBinaryClipboardToggle}
                          aria-pressed={enableBinaryClipboard}
                          disabled={!serverSettings || serverSettings.enable_binary_clipboard?.locked}
                          title={t(enableBinaryClipboard ? "buttons.binaryClipboardDisableTitle" : "buttons.binaryClipboardEnableTitle")}
                        >
                          <span className="toggle-button-sidebar-knob"></span>
                        </button>
                      </div>
                    )}
                    <div className="dashboard-clipboard-item">
                      <label htmlFor="dashboardClipboardTextarea">
                        {t("sections.clipboard.label")}
                      </label>
                      <textarea
                        className="allow-native-input"
                        id="dashboardClipboardTextarea"
                        value={dashboardClipboardContent}
                        onChange={handleClipboardChange}
                        onBlur={handleClipboardBlur}
                        rows="5"
                        placeholder={t("sections.clipboard.placeholder")}
                      />
                    </div>
                  </div>
                )}
              </div>
            )}
          </>
        )}

        {!isSecondaryDisplay && (
          <>
            {(renderableSettings.files ?? true) && (
              <div className="sidebar-section">
                <div
                  className="sidebar-section-header"
                  onClick={() => toggleSection("files")}
                  role="button"
                  aria-expanded={sectionsOpen.files}
                  aria-controls="files-content"
                  tabIndex="0"
                  onKeyDown={(e) =>
                    (e.key === "Enter" || e.key === " ") && toggleSection("files")
                  }
                >
                  <h3>{t("sections.files.title")}</h3>
                  <span className="section-toggle-icon">
                    {sectionsOpen.files ? <CaretUpIcon /> : <CaretDownIcon />}
                  </span>
                </div>
                {sectionsOpen.files && (
                  <div className="sidebar-section-content" id="files-content">
                    {(renderableSettings.fileUpload ?? true) && (
                      <button
                        className="resolution-button"
                        onClick={handleUploadClick}
                        style={{ marginTop: "5px", marginBottom: "5px" }}
                        title={t("sections.files.uploadButtonTitle")}
                      >
                        {t("sections.files.uploadButton")}
                      </button>
                    )}
                    {(renderableSettings.fileDownload ?? true) && (
                      <button
                        className="resolution-button"
                        onClick={toggleFilesModal}
                        style={{ marginTop: "5px", marginBottom: "5px" }}
                        title={t(
                          "sections.files.downloadButtonTitle",
                          "Download Files"
                        )}
                      >
                        {t("sections.files.downloadButtonTitle", "Download Files")}
                      </button>
                    )}
                  </div>
                )}
              </div>
            )}

            {(renderableSettings.apps ?? true) && (
              <div className="sidebar-section">
                <div
                  className="sidebar-section-header"
                  onClick={() => toggleSection("apps")}
                  role="button"
                  aria-expanded={sectionsOpen.apps}
                  aria-controls="apps-content"
                  tabIndex="0"
                  onKeyDown={(e) =>
                    (e.key === "Enter" || e.key === " ") && toggleSection("apps")
                  }
                >
                  <h3>{t("sections.apps.title", "Apps")}</h3>
                  <span className="section-toggle-icon">
                    {sectionsOpen.apps ? <CaretUpIcon /> : <CaretDownIcon />}
                  </span>
                </div>
                {sectionsOpen.apps && (
                  <div className="sidebar-section-content" id="apps-content">
                    <button
                      className="resolution-button"
                      onClick={toggleAppsModal}
                      style={{ marginTop: "5px", marginBottom: "5px" }}
                      title={t("sections.apps.openButtonTitle", "Manage Apps")}
                    >
                      <AppsIcon />
                      <span style={{ marginLeft: "8px" }}>
                        {t("sections.apps.openButton", "Manage Apps")}
                      </span>
                    </button>
                  </div>
                )}
              </div>
            )}

            {(renderableSettings.sharing ?? true) && (renderableSettings.enableSharing ?? true) && (
              <div className="sidebar-section">
                <div
                  className="sidebar-section-header"
                  onClick={() => toggleSection("sharing")}
                  role="button"
                  aria-expanded={sectionsOpen.sharing}
                  aria-controls="sharing-content"
                  tabIndex="0"
                  onKeyDown={(e) =>
                    (e.key === "Enter" || e.key === " ") &&
                    toggleSection("sharing")
                  }
                >
                  <h3>{t("sections.sharing.title", "Sharing")}</h3>
                  <span className="section-toggle-icon">
                    {sectionsOpen.sharing ? <CaretUpIcon /> : <CaretDownIcon />}
                  </span>
                </div>
                {sectionsOpen.sharing && (
                  <div className="sidebar-section-content" id="sharing-content">
                    {filteredSharingLinks.map((link) => {
                      const fullUrl = `${baseUrl}${link.hash}`;
                      return (
                        <div
                          key={link.id}
                          className="sharing-link-item"
                          title={link.tooltip}
                        >
                          <span className="sharing-link-label">
                            {link.label}
                          </span>
                          <div className="sharing-link-actions">
                            <a
                              href={fullUrl}
                              target="_blank"
                              rel="noopener noreferrer"
                              className="sharing-link"
                              title={`Open ${link.label} link in new tab`}
                            >
                              {fullUrl}
                            </a>
                            <button
                              type="button"
                              onClick={() => handleCopyLink(fullUrl, link.label)}
                              className="copy-button"
                              title={`Copy ${link.label} link`}
                            >
                              <CopyIcon />
                            </button>
                          </div>
                        </div>
                      );
                    })}
                  </div>
                )}
              </div>
            )}

            {(renderableSettings.gamepads ?? true) && (
              <div className="sidebar-section">
                <div
                  className="sidebar-section-header"
                  onClick={() => toggleSection("gamepads")}
                  role="button"
                  aria-expanded={sectionsOpen.gamepads}
                  aria-controls="gamepads-content"
                  tabIndex="0"
                  onKeyDown={(e) =>
                    (e.key === "Enter" || e.key === " ") &&
                    toggleSection("gamepads")
                  }
                >
                  <h3>{t("sections.gamepads.title", "Gamepads")}</h3>
                  <span className="section-toggle-icon" aria-hidden="true">
                    {sectionsOpen.gamepads ? <CaretUpIcon /> : <CaretDownIcon />}
                  </span>
                </div>
                {sectionsOpen.gamepads && (
                  <div className="sidebar-section-content" id="gamepads-content">
                    <div
                      className="dev-setting-item"
                      style={{ marginBottom: "10px" }}
                    >
                      <button
                        className={`resolution-button toggle-button ${
                          isTouchGamepadActive ? "active" : ""
                        }`}
                        onClick={handleToggleTouchGamepad}
                        title={t(
                          isTouchGamepadActive
                            ? "sections.gamepads.touchDisableTitle"
                            : "sections.gamepads.touchEnableTitle",
                          isTouchGamepadActive
                            ? "Disable Touch Gamepad"
                            : "Enable Touch Gamepad"
                        )}
                      >
                        <GamepadIcon />
                        <span style={{ marginLeft: "8px" }}>
                          {t(
                            isTouchGamepadActive
                              ? "sections.gamepads.touchActiveLabel"
                              : "sections.gamepads.touchInactiveLabel",
                            isTouchGamepadActive
                              ? "Touch Gamepad: ON"
                              : "Touch Gamepad: OFF"
                          )}
                        </span>
                      </button>
                    </div>

                    {isMobile && isTouchGamepadActive ? (
                      <p>
                        {t(
                          "sections.gamepads.physicalHiddenForTouch",
                          "Physical gamepad display is hidden while touch gamepad is active."
                        )}
                      </p>
                    ) : (
                      <>
                        {Object.keys(gamepadStates).length > 0 ? (
                          Object.keys(gamepadStates)
                            .sort((a, b) => parseInt(a, 10) - parseInt(b, 10))
                            .map((gpIndexStr) => {
                              const gpIndex = parseInt(gpIndexStr, 10);
                              return (
                                <GamepadVisualizer
                                  key={gpIndex}
                                  gamepadIndex={gpIndex}
                                  gamepadState={gamepadStates[gpIndex]}
                                />
                              );
                            })
                        ) : (
                          <p className="no-gamepads-message">
                            {isMobile
                              ? t(
                                  "sections.gamepads.noActivityMobileOrEnableTouch",
                                  "No physical gamepads. Enable touch gamepad or connect a controller."
                                )
                              : t(
                                  "sections.gamepads.noActivity",
                                  "No physical gamepad activity detected."
                                )}
                          </p>
                        )}
                      </>
                    )}
                  </div>
                )}
              </div>
            )}
          </>
        )}
      </div>


      {hoveredItem && (
        <div
          className="gauge-tooltip"
          style={{
            left: `${tooltipPosition.x}px`,
            top: `${tooltipPosition.y}px`,
          }}
        >
          {getTooltipContent(hoveredItem)}
        </div>
      )}

      <div className={`notification-container theme-${theme}`}>
        {notifications.map((n) => (
          <div
            key={n.id}
            className={`notification-item ${n.status} ${
              n.fadingOut ? "fade-out" : ""
            }`}
            role="alert"
            aria-live="polite"
          >
            <div className="notification-header">
              <span className="notification-filename" title={n.fileName}>
                {n.fileName}
              </span>
              <button
                className="notification-close-button"
                onClick={() => removeNotification(n.id)}
                aria-label={t("notifications.closeButtonAlt", {
                  fileName: n.fileName,
                })}
              >
                &times;
              </button>
            </div>
            <div className="notification-body">
              {n.status === "progress" && (
                <>
                  <span className="notification-status-text">
                    {t("notifications.uploading", { progress: n.progress })}
                  </span>
                  <div className="notification-progress-bar-outer">
                    <div
                      className="notification-progress-bar-inner"
                      style={{ width: `${n.progress}%` }}
                    />
                  </div>
                </>
              )}
              {n.status === "end" && (
                <>
                  <span className="notification-status-text">
                    {n.message ? n.message : t("notifications.uploadComplete")}
                  </span>
                  <div className="notification-progress-bar-outer">
                    <div
                      className="notification-progress-bar-inner"
                      style={{ width: `100%` }}
                    />
                  </div>
                </>
              )}
              {n.status === "error" && (
                <>
                  <span className="notification-status-text error-text">
                    {t("notifications.uploadFailed")}
                  </span>
                  <div className="notification-progress-bar-outer">
                    <div
                      className="notification-progress-bar-inner"
                      style={{ width: `100%` }}
                    />
                  </div>
                  {n.message && (
                    <p className="notification-error-message">{n.message}</p>
                  )}
                </>
              )}
              {n.status === "warn" && (
                <>
                  {" "}
                  <span className="notification-status-text warn-text">
                    {n.message ? n.message : t("notifications.warningPrefix")}
                  </span>{" "}
                </>
              )}
            </div>
          </div>
        ))}
      </div>

      {isFilesModalOpen && (
        <div className="files-modal">
          <button
            className="files-modal-close"
            onClick={toggleFilesModal}
            aria-label="Close files modal"
          >
            &times;
          </button>
          <iframe src="./files/" title="Downloadable Files" />
        </div>
      )}
      {isAppsModalOpen && (
        <AppsModal isOpen={isAppsModalOpen} onClose={toggleAppsModal} t={t} />
      )}

      {(isMobile || hasDetectedTouch) && isKeyboardButtonVisible && (renderableSettings.keyboardButton ?? true) && (
        <button
          className={`virtual-keyboard-button theme-${theme} allow-native-input`}
          onClick={onKeyboardButtonClick}
          onPointerDown={handlePointerDown}
          onPointerMove={handlePointerMove}
          onPointerUp={handlePointerUp}
          onPointerCancel={handlePointerUp}
          style={{
            position: 'fixed',
            right: `${keyboardButtonPosition.right}px`,
            bottom: `${keyboardButtonPosition.bottom}px`,
            touchAction: 'none',
          }}
          title={t("buttons.virtualKeyboardButtonTitle", "Pop Keyboard")}
          aria-label={t("buttons.virtualKeyboardButtonTitle", "Pop Keyboard")}
        >
          <KeyboardIcon />
        </button>
      )}
    </>
  );
}

export default Sidebar;
