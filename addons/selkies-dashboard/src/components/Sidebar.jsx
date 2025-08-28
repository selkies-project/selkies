// src/components/Sidebar.jsx
import React, { useState, useEffect, useCallback, useRef } from "react";
import GamepadVisualizer from "./GamepadVisualizer";
import { getTranslator } from "../translations";
import yaml from "js-yaml";

// --- Constants ---
const encoderOptions = [
  "x264enc",
  "x264enc-striped",
  "jpeg",
];

const encoderWebrtcOptions = [
  "x264enc",
  "nvh264enc",
  "openh264enc",
  "x265enc",
  "vp8enc",
  "vp9enc",
  "svtav1enc",
  "av1enc",
  "rav1enc"
];

const framerateOptions = [
  8, 12, 15, 24, 25, 30, 48, 50, 60, 90, 100, 120, 144,
];

const videoBitrateOptions = [
  1000, 2000, 4000, 8000, 10000, 12000, 14000, 16000, 18000, 20000, 25000,
  30000, 35000, 40000, 45000, 50000, 60000, 70000, 80000, 90000, 100000,
];

const audioBitrateOptions = [
  24000, 32000, 48000, 64000, 96000, 128000, 192000, 256000, 320000, 510000,
];

const videoBufferOptions = Array.from({ length: 16 }, (_, i) => i);

const videoCRFOptions = [...[50, 45, 40, 35, 30], ...Array.from({ length: 12 }, (_, i) => 28 - i), ...[15, 10, 5]];

const jpegQualityOptions = Array.from({ length: (100 - 5) / 5 + 1 }, (_, i) => 5 + i * 5);
const paintOverJpegQualityOptions = Array.from({ length: (100 - 5) / 5 + 1 }, (_, i) => 5 + i * 5);

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
const DEFAULT_VIDEO_BITRATE = 8000;
const DEFAULT_JPEG_QUALITY = 60;
const DEFAULT_PAINT_OVER_JPEG_QUALITY = 90;
const DEFAULT_USE_CPU = false;
const DEFAULT_H264_PAINTOVER_CRF = 18;
const DEFAULT_USE_PAINT_OVER_QUALITY = true;
const DEFAULT_VIDEO_BUFFER_SIZE = 0;
const DEFAULT_ENCODER = encoderOptions[0];
const DEFAULT_VIDEO_CRF = 25;
const DEFAULT_SCALE_LOCALLY = true;
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
const DEFAULT_WEBRTC_ENCODER = encoderWebrtcOptions[0];
const DEFAULT_AUDIO_BITRATE = audioBitrateOptions[3];

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
const getPrefixedKey = (key) => `${storageAppName}_${key}`;

function Sidebar({ isOpen }) {
  const [langCode, setLangCode] = useState("en");
  const [translator, setTranslator] = useState(() => getTranslator("en"));
  useEffect(() => {
    window.postMessage({ type: 'sidebarVisibilityChanged', isOpen }, window.location.origin);
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
    localStorage.getItem(getPrefixedKey("streamMode")) || DEFAULT_STREAM_MODE
  );
  const [encoderRTC, setEncoderRTC] = useState(
    localStorage.getItem(getPrefixedKey("encoderRTC")) || DEFAULT_WEBRTC_ENCODER
  );
  const [dynamicEncoderOptions, setDynamicEncoderOptions] = useState();
  const [audioBitRate, setAudioBitRate] = useState(
    parseInt(localStorage.getItem("audioBitRate"), 10) || DEFAULT_AUDIO_BITRATE
  );
  const [theme, setTheme] = useState(localStorage.getItem("theme") || "dark");
  const [encoder, setEncoder] = useState(
    localStorage.getItem(getPrefixedKey("encoder")) || DEFAULT_ENCODER
  );
  const [framerate, setFramerate] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("videoFramerate")), 10) ||
      DEFAULT_FRAMERATE
  );
  const [videoBitRate, setVideoBitRate] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("videoBitRate")), 10) ||
      DEFAULT_VIDEO_BITRATE
  );
  const [videoBufferSize, setVideoBufferSize] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("videoBufferSize")), 10) ||
      DEFAULT_VIDEO_BUFFER_SIZE
  );
  const [videoCRF, setVideoCRF] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("videoCRF")), 10) ||
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
  const [jpegQuality, setJpegQuality] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("jpegQuality")), 10) ||
      DEFAULT_JPEG_QUALITY
  );
  const [paintOverJpegQuality, setPaintOverJpegQuality] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("paintOverJpegQuality")), 10) ||
      DEFAULT_PAINT_OVER_JPEG_QUALITY
  );
  const [useCpu, setUseCpu] = useState(
    localStorage.getItem(getPrefixedKey("useCpu")) === "true"
  );
  const [h264StreamingMode, setH264StreamingMode] = useState(
    localStorage.getItem(getPrefixedKey("h264_streaming_mode")) === "true"
  );
  const [selectedDpi, setSelectedDpi] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("SCALING_DPI")), 10) || DEFAULT_SCALING_DPI
  );
  const [manualWidth, setManualWidth] = useState(localStorage.getItem(getPrefixedKey("manualWidth")) || "");
  const [manualHeight, setManualHeight] = useState(localStorage.getItem(getPrefixedKey("manualHeight")) || "");
  const [scaleLocally, setScaleLocally] = useState(() => {
    const saved = localStorage.getItem(getPrefixedKey("scaleLocallyManual"));
    return saved !== null ? saved === "true" : DEFAULT_SCALE_LOCALLY;
  });
  const [hidpiEnabled, setHidpiEnabled] = useState(() => {
    const saved = localStorage.getItem(getPrefixedKey("useCssScaling"));
    return saved !== "true";
  });
  const [antiAliasing, setAntiAliasing] = useState(() => {
    const saved = localStorage.getItem(getPrefixedKey("antiAliasingEnabled"));
    return saved !== null ? saved === "true" : true;
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

  const debouncedUpdateFramerateSettings = useCallback(
    debounce((newFramerate) => {
      window.postMessage(
        { type: "settings", settings: { videoFramerate: newFramerate } },
        window.location.origin
      );
    }, DEBOUNCE_DELAY),
    []
  );

  const debouncedUpdateVideoBitrateSettings = useCallback(
    debounce((newBitrate) => {
      window.postMessage(
        { type: "settings", settings: { videoBitRate: newBitrate } },
        window.location.origin
      );
    }, DEBOUNCE_DELAY),
    []
  );

  const debouncedUpdateAudioBitrateSettings = useCallback(
    debounce((newAudioBitrate) => {
      window.postMessage(
        { type: "settings", settings: { audioBitRate: newAudioBitrate} },
      );
    }, DEBOUNCE_DELAY),
    []
  )

  const debouncedUpdateVideoBufferSizeSettings = useCallback(
    debounce((newSize) => {
      window.postMessage(
        { type: "settings", settings: { videoBufferSize: newSize } },
        window.location.origin
      );
    }, DEBOUNCE_DELAY),
    []
  );

  const debouncedUpdateVideoCRFSettings = useCallback(
    debounce((newCRF) => {
      window.postMessage(
        { type: "settings", settings: { videoCRF: newCRF } },
        window.location.origin
      );
    }, DEBOUNCE_DELAY),
    []
  );

  const debouncedUpdateH264PaintoverCRFSettings = useCallback(
    debounce((newCRF) => {
      window.postMessage(
        { type: "settings", settings: { h264_paintover_crf: newCRF } },
        window.location.origin
      );
    }, DEBOUNCE_DELAY),
    []
  );

  const debouncedUpdateUsePaintOverQualitySettings = useCallback(
    debounce((newVal) => {
      window.postMessage(
        { type: "settings", settings: { use_paint_over_quality: newVal } },
        window.location.origin
      );
    }, DEBOUNCE_DELAY),
    []
  );

  const debouncedUpdateH264FullColorSettings = useCallback(
    debounce((newFullColor) => {
      window.postMessage(
        { type: "settings", settings: { h264_fullcolor: newFullColor } },
        window.location.origin
      );
    }, DEBOUNCE_DELAY),
    []
  );

  const debouncedUpdateH264StreamingModeSettings = useCallback(
    debounce((newStreamingMode) => {
      window.postMessage(
        { type: "settings", settings: { h264_streaming_mode: newStreamingMode } },
        window.location.origin
      );
    }, DEBOUNCE_DELAY),
    []
  );

  const debouncedUpdateJpegQualitySettings = useCallback(
    debounce((newQuality) => {
      window.postMessage(
        { type: "settings", settings: { jpegQuality: newQuality } },
        window.location.origin
      );
    }, DEBOUNCE_DELAY),
    []
  );

  const debouncedUpdatePaintOverJpegQualitySettings = useCallback(
    debounce((newQuality) => {
      window.postMessage(
        { type: "settings", settings: { paintOverJpegQuality: newQuality } },
        window.location.origin
      );
    }, DEBOUNCE_DELAY),
    []
  );

  const debouncedUpdateUseCpuSettings = useCallback(
    debounce((newUseCpu) => {
      window.postMessage(
        { type: "settings", settings: { useCpu: newUseCpu } },
        window.location.origin
      );
    }, DEBOUNCE_DELAY),
    []
  );

  const handleDpiScalingChange = (event) => {
    const newDpi = parseInt(event.target.value, 10);
    setSelectedDpi(newDpi);
    window.postMessage(
      { type: "settings", settings: { SCALING_DPI: newDpi } },
      window.location.origin
    );

    const notificationId = `scaling-action-required-${Date.now()}`;
    const title = t("notifications.scalingTitle", "Scaling Updated: Action Required");
    const message = t(
      "notifications.scalingMessage", 
      "New scaling applied. Some applications or desktop environments might require a restart."
    );

    setNotifications(prev => {
      const newNotifs = [...prev, {
        id: notificationId,
        fileName: title,
        status: 'end',
        message: message,
        timestamp: Date.now(),
        fadingOut: false,
      }];
      return newNotifs.slice(-MAX_NOTIFICATIONS);
    });

    scheduleNotificationRemoval(notificationId, NOTIFICATION_TIMEOUT_ERROR); 
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
    window.postMessage(
      { type: "settings", settings: { encoder: selectedEncoder } },
      window.location.origin
    );
  };
  const handleFramerateChange = (event) => {
    const index = parseInt(event.target.value, 10);
    const selectedFramerate = framerateOptions[index];
    if (selectedFramerate !== undefined) {
      setFramerate(selectedFramerate);
      debouncedUpdateFramerateSettings(selectedFramerate);
    }
  };
  const handleJpegQualityChange = (event) => {
    const index = parseInt(event.target.value, 10);
    const selectedQuality = jpegQualityOptions[index];
    if (selectedQuality !== undefined) {
      setJpegQuality(selectedQuality);
      debouncedUpdateJpegQualitySettings(selectedQuality);
    }
  };
  const handlePaintOverJpegQualityChange = (event) => {
    const index = parseInt(event.target.value, 10);
    const selectedQuality = paintOverJpegQualityOptions[index];
    if (selectedQuality !== undefined) {
      setPaintOverJpegQuality(selectedQuality);
      debouncedUpdatePaintOverJpegQualitySettings(selectedQuality);
    }
  };
  const handleVideoBitrateChange = (event) => {
    const index = parseInt(event.target.value, 10);
    const selectedBitrate = videoBitrateOptions[index];
    if (selectedBitrate !== undefined) {
      setVideoBitRate(selectedBitrate);
      debouncedUpdateVideoBitrateSettings(selectedBitrate);
    }
  };
  const handleAudioBitrateChange = (event) => {
    console.log("audio bitrate trigger: ", event.target.value);
    const index = parseInt(event.target.value, 10);
    const selectedAudioBitrate = audioBitrateOptions[index];
    if (selectedAudioBitrate !== undefined) {
      setAudioBitRate(selectedAudioBitrate);
      debouncedUpdateAudioBitrateSettings(selectedAudioBitrate);
    }
  }
  const handleVideoBufferSizeChange = (event) => {
    const index = parseInt(event.target.value, 10);
    const selectedSize = videoBufferOptions[index];
    if (selectedSize !== undefined) {
      setVideoBufferSize(selectedSize);
      debouncedUpdateVideoBufferSizeSettings(selectedSize);
    }
  };
  const handleVideoCRFChange = (event) => {
    const index = parseInt(event.target.value, 10);
    const selectedCRF = videoCRFOptions[index];
    if (selectedCRF !== undefined) {
      setVideoCRF(selectedCRF);
      debouncedUpdateVideoCRFSettings(selectedCRF);
    }
  };
  const handleH264PaintoverCRFChange = (event) => {
    const index = parseInt(event.target.value, 10);
    const selectedCRF = videoCRFOptions[index];
    if (selectedCRF !== undefined) {
      setH264PaintoverCRF(selectedCRF);
      debouncedUpdateH264PaintoverCRFSettings(selectedCRF);
    }
  };
  const handleH264FullColorToggle = () => {
    const newFullColorState = !h264FullColor;
    setH264FullColor(newFullColorState);
    debouncedUpdateH264FullColorSettings(newFullColorState);
  };
  const handleUsePaintOverQualityToggle = () => {
    const newUsePaintOverQualityState = !usePaintOverQuality;
    setUsePaintOverQuality(newUsePaintOverQualityState);
    debouncedUpdateUsePaintOverQualitySettings(newUsePaintOverQualityState);
  };
  const handleUseCpuToggle = () => {
    const newUseCpuState = !useCpu;
    setUseCpu(newUseCpuState);
    debouncedUpdateUseCpuSettings(newUseCpuState);
  };
  const handleH264StreamingModeToggle = () => {
    const newStreamingModeState = !h264StreamingMode;
    setH264StreamingMode(newStreamingModeState);
    debouncedUpdateH264StreamingModeSettings(newStreamingModeState);
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
  const handleSetManualResolution = () => {
    const width = parseInt(manualWidth.trim(), 10),
      height = parseInt(manualHeight.trim(), 10);
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
      }  catch (error) {
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
    (streamMode === STREAM_MODE_WEBRTC ? setDynamicEncoderOptions(encoderWebrtcOptions): 
      setDynamicEncoderOptions(encoderOptions));
  }, [streamMode]);

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

  const sidebarClasses = `sidebar ${isOpen ? "is-open" : ""} theme-${theme}`;
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
  const isWebrtc = streamMode === STREAM_MODE_WEBRTC;

  return (
    <>
      <div className={sidebarClasses}>
        <div className="sidebar-header">
          <a
            href="https://github.com/selkies-project/selkies"
            target="_blank"
            rel="noopener noreferrer"
          >
            <SelkiesLogo width={30} height={30} t={t} />
          </a>
          <a
            href="https://github.com/selkies-project/selkies"
            target="_blank"
            rel="noopener noreferrer"
          >
            <h2>{t("selkiesTitle")}</h2>
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
            <button
              className="header-action-button fullscreen-button"
              onClick={handleBrowserFullscreen}
              title={t("fullscreenTitle")}
            >
              <FullscreenIcon />
            </button>
            {(isMobile || hasDetectedTouch) ? (
              <button
                className={`header-action-button trackpad-mode-button ${isTrackpadModeActive ? "active" : ""}`}
                onClick={handleToggleTrackpadMode}
                title={t("trackpadModeTitle", "Trackpad Mode")}
              >
                <TrackpadIcon />
              </button>
            ) : (
              <button
                className="header-action-button gaming-mode-button"
                onClick={handleFullscreenRequest}
                title={t("gamingModeTitle", "Gaming Mode")}
              >
                <GamingModeIcon />
              </button>
            )}
          </div>
        </div>

        <div className="sidebar-action-buttons">
          <button
            className={`action-button ${isVideoActive ? "active" : ""}`}
            onClick={handleVideoToggle}
            title={t(
              isVideoActive
                ? "buttons.videoStreamDisableTitle"
                : "buttons.videoStreamEnableTitle"
            )}
          >
            {" "}
            <ScreenIcon />{" "}
          </button>
          <button
            className={`action-button ${isAudioActive ? "active" : ""}`}
            onClick={handleAudioToggle}
            title={t(
              isAudioActive
                ? "buttons.audioStreamDisableTitle"
                : "buttons.audioStreamEnableTitle"
            )}
          >
            {" "}
            <SpeakerIcon />{" "}
          </button>
          <button
            className={`action-button ${isMicrophoneActive ? "active" : ""}`}
            onClick={handleMicrophoneToggle}
            title={t(
              isMicrophoneActive
                ? "buttons.microphoneDisableTitle"
                : "buttons.microphoneEnableTitle"
            )}
          >
            {" "}
            <MicrophoneIcon />{" "}
          </button>
          <button
            className={`action-button ${isGamepadEnabled ? "active" : ""}`}
            onClick={handleGamepadToggle}
            title={t(
              isGamepadEnabled
                ? "buttons.gamepadDisableTitle"
                : "buttons.gamepadEnableTitle"
            )}
          >
            {" "}
            <GamepadIcon />{" "}
          </button>
        </div>

        {(isMobile || hasDetectedTouch) && (
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
            <h3>{t("sections.video.title")}</h3>{" "}
            <span className="section-toggle-icon">
              {sectionsOpen.settings ? <CaretUpIcon /> : <CaretDownIcon />}
            </span>
          </div>
          {sectionsOpen.settings && (
            <div className="sidebar-section-content" id="settings-content">
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
              <div className="dev-setting-item">
                {" "}
                <label htmlFor="encoderSelect">
                  {t("sections.video.encoderLabel")}
                </label>{" "}
                <select
                  id="encoderSelect"
                  value={isWebrtc? encoderRTC: encoder}
                  onChange={handleEncoderChange}
                >
                  {" "}
                  {dynamicEncoderOptions.map((enc) => (
                    <option key={enc} value={enc}>
                      {enc}
                    </option>
                  ))}{" "}
                </select>{" "}
              </div>
              {(showFPS || isWebrtc) && (
                <div className="dev-setting-item">
                  {" "}
                  <label htmlFor="framerateSlider">
                    {t("sections.video.framerateLabel", {
                      framerate: framerate,
                    })}
                  </label>{" "}
                  <input
                    type="range"
                    id="framerateSlider"
                    min="0"
                    max={framerateOptions.length - 1}
                    step="1"
                    value={framerateOptions.indexOf(framerate)}
                    onChange={handleFramerateChange}
                  />{" "}
                </div>
              )}
              {(!isWebrtc && showJpegOptions) && (
                <>
                  <div className="dev-setting-item">
                    <label htmlFor="jpegQualitySlider">
                      {t("sections.video.jpegQualityLabel", {
                        jpegQuality: jpegQuality,
                      })}
                    </label>
                    <input
                      type="range"
                      id="jpegQualitySlider"
                      min="0"
                      max={jpegQualityOptions.length - 1}
                      step="1"
                      value={jpegQualityOptions.indexOf(jpegQuality)}
                      onChange={handleJpegQualityChange}
                    />
                  </div>
                  <div className="dev-setting-item">
                    <label htmlFor="paintOverJpegQualitySlider">
                      {t("sections.video.paintOverJpegQualityLabel", {
                        paintOverJpegQuality: paintOverJpegQuality,
                      })}
                    </label>
                    <input
                      type="range"
                      id="paintOverJpegQualitySlider"
                      min="0"
                      max={paintOverJpegQualityOptions.length - 1}
                      step="1"
                      value={paintOverJpegQualityOptions.indexOf(paintOverJpegQuality)}
                      onChange={handlePaintOverJpegQualityChange}
                    />
                  </div>
                </>
              )}
              {(showBitrate || isWebrtc) && (
                <div className="dev-setting-item">
                  {" "}
                  <label htmlFor="videoBitrateSlider">
                    {t("sections.video.bitrateLabel", {
                      bitrate: videoBitRate / 1000,
                    })}
                  </label>{" "}
                  <input
                    type="range"
                    id="videoBitrateSlider"
                    min="0"
                    max={videoBitrateOptions.length - 1}
                    step="1"
                    value={videoBitrateOptions.indexOf(videoBitRate)}
                    onChange={handleVideoBitrateChange}
                  />{" "}
                </div>
              )}
              {!isWebrtc && showBufferSize && (
                <div className="dev-setting-item">
                  {" "}
                  <label htmlFor="videoBufferSizeSlider">
                    {videoBufferSize === 0
                      ? t("sections.video.bufferLabelImmediate")
                      : t("sections.video.bufferLabelFrames", {
                          videoBufferSize: videoBufferSize,
                        })}
                  </label>{" "}
                  <input
                    type="range"
                    id="videoBufferSizeSlider"
                    min="0"
                    max={videoBufferOptions.length - 1}
                    step="1"
                    value={videoBufferOptions.indexOf(videoBufferSize)}
                    onChange={handleVideoBufferSizeChange}
                  />{" "}
                </div>
              )}
              {(!isWebrtc && showCRF) && (
                <div className="dev-setting-item">
                  {" "}
                  <label htmlFor="videoCRFSlider">
                    {t("sections.video.crfLabel", { crf: videoCRF })}
                  </label>{" "}
                  <input
                    type="range"
                    id="videoCRFSlider"
                    min="0"
                    max={videoCRFOptions.length - 1}
                    step="1"
                    value={videoCRFOptions.indexOf(videoCRF)}
                    onChange={handleVideoCRFChange}
                  />{" "}
                </div>
              )}
              {(!isWebrtc && showCRF) && (
                <div className="dev-setting-item">
                  {" "}
                  <label htmlFor="h264PaintoverCRFSlider">
                    {t("sections.video.paintoverCrfLabel", { crf: h264PaintoverCRF }, `Paint-Over CRF: ${h264PaintoverCRF}`)}
                  </label>{" "}
                  <input
                    type="range"
                    id="h264PaintoverCRFSlider"
                    min="0"
                    max={videoCRFOptions.length - 1}
                    step="1"
                    value={videoCRFOptions.indexOf(h264PaintoverCRF)}
                    onChange={handleH264PaintoverCRFChange}
                  />{" "}
                </div>
              )}
              {(!isWebrtc && showPaintOverQualityToggle) && (
                <div className="dev-setting-item toggle-item">
                  <label htmlFor="usePaintOverQualityToggle">
                    {t("sections.video.usePaintOverQualityLabel", "Use Paint-Over Quality")}
                  </label>
                  <button
                    id="usePaintOverQualityToggle"
                    className={`toggle-button-sidebar ${usePaintOverQuality ? "active" : ""}`}
                    onClick={handleUsePaintOverQualityToggle}
                    aria-pressed={usePaintOverQuality}
                    title={t(usePaintOverQuality ? "buttons.usePaintOverQualityDisableTitle" : "buttons.usePaintOverQualityEnableTitle",
                               usePaintOverQuality ? "Disable Paint-Over Quality" : "Enable Paint-Over Quality")}
                  >
                    <span className="toggle-button-sidebar-knob"></span>
                  </button>
                </div>
              )}
              {!isWebrtc && showH264Options && (
                <div className="dev-setting-item toggle-item">
                  <label 
                    htmlFor="h264StreamingModeToggle"
                    title={t("sections.video.streamingModeDetails", "Turbo mode disables all VNC logic and encodes all frames like a traditional video encoder.")}
                  >
                    {t("sections.video.streamingModeLabel", "Turbo")}
                  </label>
                  <button
                    id="h264StreamingModeToggle"
                    className={`toggle-button-sidebar ${h264StreamingMode ? "active" : ""}`}
                    onClick={handleH264StreamingModeToggle}
                    aria-pressed={h264StreamingMode}
                    title={t(h264StreamingMode ? "buttons.h264StreamingModeDisableTitle" : "buttons.h264StreamingModeEnableTitle",
                               h264StreamingMode ? "Disable Turbo" : "Enable Turbo")}
                  >
                    <span className="toggle-button-sidebar-knob"></span>
                  </button>
                </div>
              )}
              {(!isWebrtc && showH264Options) && (
                <div className="dev-setting-item toggle-item">
                  <label htmlFor="h264FullColorToggle">
                    {t("sections.video.fullColorLabel")}
                  </label>
                  <button
                    id="h264FullColorToggle"
                    className={`toggle-button-sidebar ${h264FullColor ? "active" : ""}`}
                    onClick={handleH264FullColorToggle}
                    aria-pressed={h264FullColor}
                    title={t(h264FullColor ? "buttons.h264FullColorDisableTitle" : "buttons.h264FullColorEnableTitle", 
                               h264FullColor ? "Disable H.264 Full Color" : "Enable H.264 Full Color")}
                  >
                    <span className="toggle-button-sidebar-knob"></span>
                  </button>
                </div>
              )}
              {(!isWebrtc && showH264Options) && (
                <div className="dev-setting-item toggle-item">
                  <label htmlFor="useCpuToggle">
                    {t("sections.video.useCpuLabel", "CPU Encoding")}
                  </label>
                  <button
                    id="useCpuToggle"
                    className={`toggle-button-sidebar ${useCpu ? "active" : ""}`}
                    onClick={handleUseCpuToggle}
                    aria-pressed={useCpu}
                    title={t(useCpu ? "buttons.useCpuDisableTitle" : "buttons.useCpuEnableTitle",
                               useCpu ? "Disable CPU Encoding" : "Enable CPU Encoding")}
                  >
                    <span className="toggle-button-sidebar-knob"></span>
                  </button>
                </div>
              )}
            </div>
          )}
        </div>

        <div className="sidebar-section">
          <div
            className="sidebar-section-header"
            onClick={() => toggleSection("audioSettings")}
            role="button"
            aria-expanded={sectionsOpen.audioSettings}
            aria-controls="audio-settings-content"
            tabIndex="0"
            onKeyDown={(e) =>
              (e.key === "Enter" || e.key === " ") &&
              toggleSection("audioSettings")
            }
          >
            <h3>{t("sections.audio.title")}</h3>{" "}
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
                {" "}
                <label htmlFor="audioInputSelect">
                  {t("sections.audio.inputLabel")}
                </label>{" "}
                <select
                  id="audioInputSelect"
                  value={selectedInputDeviceId}
                  onChange={handleAudioInputChange}
                  disabled={isLoadingAudioDevices || !!audioDeviceError}
                  className="audio-device-select"
                >
                  {" "}
                  {audioInputDevices.map((d) => (
                    <option key={d.deviceId} value={d.deviceId}>
                      {d.label}
                    </option>
                  ))}{" "}
                </select>{" "}
              </div>
              {isOutputSelectionSupported && (
                <div className="dev-setting-item">
                  {" "}
                  <label htmlFor="audioOutputSelect">
                    {t("sections.audio.outputLabel")}
                  </label>{" "}
                  <select
                    id="audioOutputSelect"
                    value={selectedOutputDeviceId}
                    onChange={handleAudioOutputChange}
                    disabled={isLoadingAudioDevices || !!audioDeviceError}
                    className="audio-device-select"
                  >
                    {" "}
                    {audioOutputDevices.map((d) => (
                      <option key={d.deviceId} value={d.deviceId}>
                        {d.label}
                      </option>
                    ))}{" "}
                  </select>{" "}
                </div>
              )}
              {isWebrtc && isOutputSelectionSupported && (
                <div className="dev-setting-item">
                  {" "}
                  <label htmlFor="audioBitrateSlider">
                    {t("sections.audio.bitrateLabel", {
                      bitrate: audioBitRate / 1000,
                    })}
                  </label>{" "}
                  <input
                    type="range"
                    id="audioBitrateSlider"
                    min="0"
                    max={audioBitrateOptions.length - 1}
                    step="1"
                    value={audioBitrateOptions.indexOf(audioBitRate)}
                    onChange={handleAudioBitrateChange}
                  />{" "}
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
            <h3>{t("sections.screen.title")}</h3>{" "}
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
              <div className="dev-setting-item">
                <label htmlFor="uiScalingSelect">
                  {t("sections.screen.uiScalingLabel", "UI Scaling")}
                </label>
                <select
                  id="uiScalingSelect"
                  value={selectedDpi}
                  onChange={handleDpiScalingChange}
                >
                  {dpiScalingOptions.map((option) => (
                    <option key={option.value} value={option.value}>
                      {option.value === currentDeviceDpi
                        ? `${option.label} *`
                        : option.label}
                    </option>
                  ))}
                </select>
              </div>
              <div className="dev-setting-item">
                {" "}
                <label htmlFor="resolutionPresetSelect">
                  {t("sections.screen.presetLabel")}
                </label>{" "}
                <select
                  id="resolutionPresetSelect"
                  value={presetValue}
                  onChange={handlePresetChange}
                >
                  {" "}
                  {translatedCommonResolutions.map((res, i) => (
                    <option key={i} value={res.value} disabled={i === 0}>
                      {res.text}
                    </option>
                  ))}{" "}
                </select>{" "}
              </div>
              <div className="resolution-manual-inputs">
                <div className="dev-setting-item manual-input-item">
                  {" "}
                  <label htmlFor="manualWidthInput">
                    {t("sections.screen.widthLabel")}
                  </label>{" "}
                  <input
                    className="allow-native-input"
                    type="number"
                    id="manualWidthInput"
                    min="1"
                    step="2"
                    placeholder={t("sections.screen.widthPlaceholder")}
                    value={manualWidth}
                    onChange={handleManualWidthChange}
                  />{" "}
                </div>
                <div className="dev-setting-item manual-input-item">
                  {" "}
                  <label htmlFor="manualHeightInput">
                    {t("sections.screen.heightLabel")}
                  </label>{" "}
                  <input
                    className="allow-native-input"
                    type="number"
                    id="manualHeightInput"
                    min="1"
                    step="2"
                    placeholder={t("sections.screen.heightPlaceholder")}
                    value={manualHeight}
                    onChange={handleManualHeightChange}
                  />{" "}
                </div>
              </div>
              <div className="resolution-action-buttons">
                {" "}
                <button
                  className="resolution-button"
                  onClick={handleSetManualResolution}
                >
                  {t("sections.screen.setManualButton")}
                </button>{" "}
                <button
                  className="resolution-button reset-button"
                  onClick={handleResetResolution}
                >
                  {t("sections.screen.resetButton")}
                </button>{" "}
              </div>
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
                {" "}
                {t("sections.screen.scaleLocallyLabel")}{" "}
                {t(
                  scaleLocally
                    ? "sections.screen.scaleLocallyOn"
                    : "sections.screen.scaleLocallyOff"
                )}{" "}
              </button>
            </div>
          )}
        </div>

        <div className="sidebar-section">
          <div
            className="sidebar-section-header"
            onClick={() => toggleSection("stats")}
            role="button"
            aria-expanded={sectionsOpen.stats}
            aria-controls="stats-content"
            tabIndex="0"
            onKeyDown={(e) =>
              (e.key === "Enter" || e.key === " ") && toggleSection("stats")
            }
          >
            <h3>{t("sections.stats.title")}</h3>{" "}
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
                  {" "}
                  <svg
                    width={gaugeSize}
                    height={gaugeSize}
                    viewBox={`0 0 ${gaugeSize} ${gaugeSize}`}
                  >
                    {" "}
                    <circle
                      stroke="var(--item-border)"
                      fill="transparent"
                      strokeWidth={gaugeStrokeWidth}
                      r={gaugeRadius}
                      cx={gaugeCenter}
                      cy={gaugeCenter}
                    />{" "}
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
                      }}
                    />{" "}
                    <text
                      x={gaugeCenter}
                      y={gaugeCenter}
                      textAnchor="middle"
                      dominantBaseline="central"
                      fontSize={`${gaugeSize / 5}px`}
                      fill="var(--sidebar-text)"
                      fontWeight="bold"
                    >
                      {" "}
                      {Math.round(
                        Math.max(0, Math.min(100, cpuPercent || 0))
                      )}%{" "}
                    </text>{" "}
                  </svg>{" "}
                  <div className="gauge-label">
                    {t("sections.stats.cpuLabel")}
                  </div>{" "}
                </div>
                <div
                  className="gauge-container"
                  onMouseEnter={(e) => handleMouseEnter(e, "sysmem")}
                  onMouseLeave={handleMouseLeave}
                >
                  {" "}
                  <svg
                    width={gaugeSize}
                    height={gaugeSize}
                    viewBox={`0 0 ${gaugeSize} ${gaugeSize}`}
                  >
                    {" "}
                    <circle
                      stroke="var(--item-border)"
                      fill="transparent"
                      strokeWidth={gaugeStrokeWidth}
                      r={gaugeRadius}
                      cx={gaugeCenter}
                      cy={gaugeCenter}
                    />{" "}
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
                      }}
                    />{" "}
                    <text
                      x={gaugeCenter}
                      y={gaugeCenter}
                      textAnchor="middle"
                      dominantBaseline="central"
                      fontSize={`${gaugeSize / 5}px`}
                      fill="var(--sidebar-text)"
                      fontWeight="bold"
                    >
                      {" "}
                      {Math.round(
                        Math.max(0, Math.min(100, sysMemPercent || 0))
                      )}
                      %{" "}
                    </text>{" "}
                  </svg>{" "}
                  <div className="gauge-label">
                    {t("sections.stats.sysMemLabel")}
                  </div>{" "}
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
                          cy={gaugeCenter}
                        />
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
                          }}
                        />
                        <text
                          x={gaugeCenter}
                          y={gaugeCenter}
                          textAnchor="middle"
                          dominantBaseline="central"
                          fontSize={`${gaugeSize / 5}px`}
                          fill="var(--sidebar-text)"
                          fontWeight="bold"
                        >
                          {" "}
                          {Math.round(
                            Math.max(0, Math.min(100, gpuPercent || 0))
                          )}%{" "}
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
                          cy={gaugeCenter}
                        />
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
                          }}
                        />
                        <text
                          x={gaugeCenter}
                          y={gaugeCenter}
                          textAnchor="middle"
                          dominantBaseline="central"
                          fontSize={`${gaugeSize / 5}px`}
                          fill="var(--sidebar-text)"
                          fontWeight="bold"
                        >
                          {" "}
                          {Math.round(
                            Math.max(0, Math.min(100, gpuMemPercent || 0))
                          )}
                          %{" "}
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
                  {" "}
                  <svg
                    width={gaugeSize}
                    height={gaugeSize}
                    viewBox={`0 0 ${gaugeSize} ${gaugeSize}`}
                  >
                    {" "}
                    <circle
                      stroke="var(--item-border)"
                      fill="transparent"
                      strokeWidth={gaugeStrokeWidth}
                      r={gaugeRadius}
                      cx={gaugeCenter}
                      cy={gaugeCenter}
                    />{" "}
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
                      }}
                    />{" "}
                    <text
                      x={gaugeCenter}
                      y={gaugeCenter}
                      textAnchor="middle"
                      dominantBaseline="central"
                      fontSize={`${gaugeSize / 5}px`}
                      fill="var(--sidebar-text)"
                      fontWeight="bold"
                    >
                      {" "}
                      {clientFps}{" "}
                    </text>{" "}
                  </svg>{" "}
                  <div className="gauge-label">
                    {t("sections.stats.fpsLabel")}
                  </div>{" "}
                </div>
                <div
                  className="gauge-container"
                  onMouseEnter={(e) => handleMouseEnter(e, "audio")}
                  onMouseLeave={handleMouseLeave}
                >
                  {" "}
                  <svg
                    width={gaugeSize}
                    height={gaugeSize}
                    viewBox={`0 0 ${gaugeSize} ${gaugeSize}`}
                  >
                    {" "}
                    <circle
                      stroke="var(--item-border)"
                      fill="transparent"
                      strokeWidth={gaugeStrokeWidth}
                      r={gaugeRadius}
                      cx={gaugeCenter}
                      cy={gaugeCenter}
                    />{" "}
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
                      }}
                    />{" "}
                    <text
                      x={gaugeCenter}
                      y={gaugeCenter}
                      textAnchor="middle"
                      dominantBaseline="central"
                      fontSize={`${gaugeSize / 5}px`}
                      fill="var(--sidebar-text)"
                      fontWeight="bold"
                    >
                      {" "}
                      {audioBuffer}{" "}
                    </text>{" "}
                  </svg>{" "}
                  <div className="gauge-label">
                    {t("sections.stats.audioLabel")}
                  </div>{" "}
                </div>
                <div
                  className="gauge-container"
                  onMouseEnter={(e) => handleMouseEnter(e, "bandwidth")}
                  onMouseLeave={handleMouseLeave}
                >
                  {" "}
                  <svg
                    width={gaugeSize}
                    height={gaugeSize}
                    viewBox={`0 0 ${gaugeSize} ${gaugeSize}`}
                  >
                    {" "}
                    <circle
                      stroke="var(--item-border)"
                      fill="transparent"
                      strokeWidth={gaugeStrokeWidth}
                      r={gaugeRadius}
                      cx={gaugeCenter}
                      cy={gaugeCenter}
                    />{" "}
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
                      }}
                    />{" "}
                    <text
                      x={gaugeCenter}
                      y={gaugeCenter}
                      textAnchor="middle"
                      dominantBaseline="central"
                      fontSize={`${gaugeSize / 5}px`}
                      fill="var(--sidebar-text)"
                      fontWeight="bold"
                    >
                      {" "}
                      {Math.round(bandwidthMbps)}{" "}
                    </text>{" "}
                  </svg>{" "}
                  <div className="gauge-label">
                    {t("sections.stats.bandwidthLabel", "Bandwidth")}
                  </div>{" "}
                </div>
                <div
                  className="gauge-container"
                  onMouseEnter={(e) => handleMouseEnter(e, "latency")}
                  onMouseLeave={handleMouseLeave}
                >
                  {" "}
                  <svg
                    width={gaugeSize}
                    height={gaugeSize}
                    viewBox={`0 0 ${gaugeSize} ${gaugeSize}`}
                  >
                    {" "}
                    <circle
                      stroke="var(--item-border)"
                      fill="transparent"
                      strokeWidth={gaugeStrokeWidth}
                      r={gaugeRadius}
                      cx={gaugeCenter}
                      cy={gaugeCenter}
                    />{" "}
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
                      }}
                    />{" "}
                    <text
                      x={gaugeCenter}
                      y={gaugeCenter}
                      textAnchor="middle"
                      dominantBaseline="central"
                      fontSize={`${gaugeSize / 5}px`}
                      fill="var(--sidebar-text)"
                      fontWeight="bold"
                    >
                      {" "}
                      {Math.round(latencyMs)}{" "}
                    </text>{" "}
                  </svg>{" "}
                  <div className="gauge-label">
                    {t("sections.stats.latencyLabel", "Latency")}
                  </div>{" "}
                </div>
              </div>
            </div>
          )}
        </div>

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
            <h3>{t("sections.clipboard.title")}</h3>{" "}
            <span className="section-toggle-icon">
              {sectionsOpen.clipboard ? <CaretUpIcon /> : <CaretDownIcon />}
            </span>
          </div>
          {sectionsOpen.clipboard && (
            <div className="sidebar-section-content" id="clipboard-content">
              {" "}
              <div className="dashboard-clipboard-item">
                {" "}
                <label htmlFor="dashboardClipboardTextarea">
                  {t("sections.clipboard.label")}
                </label>{" "}
                <textarea
                  className="allow-native-input"
                  id="dashboardClipboardTextarea"
                  value={dashboardClipboardContent}
                  onChange={handleClipboardChange}
                  onBlur={handleClipboardBlur}
                  rows="5"
                  placeholder={t("sections.clipboard.placeholder")}
                />{" "}
              </div>{" "}
            </div>
          )}
        </div>

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
            <h3>{t("sections.files.title")}</h3>{" "}
            <span className="section-toggle-icon">
              {sectionsOpen.files ? <CaretUpIcon /> : <CaretDownIcon />}
            </span>
          </div>
          {sectionsOpen.files && (
            <div className="sidebar-section-content" id="files-content">
              {" "}
              <button
                className="resolution-button"
                onClick={handleUploadClick}
                style={{ marginTop: "5px", marginBottom: "5px" }}
                title={t("sections.files.uploadButtonTitle")}
              >
                {" "}
                {t("sections.files.uploadButton")}{" "}
              </button>{" "}
              <button
                className="resolution-button"
                onClick={toggleFilesModal}
                style={{ marginTop: "5px", marginBottom: "5px" }}
                title={t(
                  "sections.files.downloadButtonTitle",
                  "Download Files"
                )}
              >
                {" "}
                {t("sections.files.downloadButtonTitle", "Download Files")}{" "}
              </button>{" "}
            </div>
          )}
        </div>

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
            <h3>{t("sections.apps.title", "Apps")}</h3>{" "}
            <span className="section-toggle-icon">
              {sectionsOpen.apps ? <CaretUpIcon /> : <CaretDownIcon />}
            </span>
          </div>
          {sectionsOpen.apps && (
            <div className="sidebar-section-content" id="apps-content">
              {" "}
              <button
                className="resolution-button"
                onClick={toggleAppsModal}
                style={{ marginTop: "5px", marginBottom: "5px" }}
                title={t("sections.apps.openButtonTitle", "Manage Apps")}
              >
                {" "}
                <AppsIcon />{" "}
                <span style={{ marginLeft: "8px" }}>
                  {t("sections.apps.openButton", "Manage Apps")}
                </span>{" "}
              </button>{" "}
            </div>
          )}
        </div>

        <div className="sidebar-section">
          <div
            className="sidebar-section-header"
            onClick={() => toggleSection("sharing")}
            role="button"
            aria-expanded={sectionsOpen.sharing}
            aria-controls="sharing-content"
            tabIndex="0"
            onKeyDown={(e) =>
              (e.key === "Enter" || e.key === " ") && toggleSection("sharing")
            }
          >
            <h3>{t("sections.sharing.title", "Sharing")}</h3>
            <span className="section-toggle-icon">
              {sectionsOpen.sharing ? <CaretUpIcon /> : <CaretDownIcon />}
            </span>
          </div>
          {sectionsOpen.sharing && (
            <div className="sidebar-section-content" id="sharing-content">
              {sharingLinks.map(link => {
                const fullUrl = `${baseUrl}${link.hash}`;
                return (
                  <div key={link.id} className="sharing-link-item" title={link.tooltip}>
                    <span className="sharing-link-label">{link.label}</span>
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
        {
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
                {
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
                }

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
        }
      </div>

      {hoveredItem && (
        <div
          className="gauge-tooltip"
          style={{
            left: `${tooltipPosition.x}px`,
            top: `${tooltipPosition.y}px`,
          }}
        >
          {" "}
          {getTooltipContent(hoveredItem)}{" "}
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
              {" "}
              <span className="notification-filename" title={n.fileName}>
                {n.fileName}
              </span>{" "}
              <button
                className="notification-close-button"
                onClick={() => removeNotification(n.id)}
                aria-label={t("notifications.closeButtonAlt", {
                  fileName: n.fileName,
                })}
              >
                &times;
              </button>{" "}
            </div>
            <div className="notification-body">
              {n.status === "progress" && (
                <>
                  {" "}
                  <span className="notification-status-text">
                    {t("notifications.uploading", { progress: n.progress })}
                  </span>{" "}
                  <div className="notification-progress-bar-outer">
                    <div
                      className="notification-progress-bar-inner"
                      style={{ width: `${n.progress}%` }}
                    />
                  </div>{" "}
                </>
              )}
              {n.status === "end" && (
                <>
                  {" "}
                  <span className="notification-status-text">
                    {n.message ? n.message : t("notifications.uploadComplete")}
                  </span>{" "}
                  <div className="notification-progress-bar-outer">
                    <div
                      className="notification-progress-bar-inner"
                      style={{ width: `100%` }}
                    />
                  </div>{" "}
                </>
              )}
              {n.status === "error" && (
                <>
                  {" "}
                  <span className="notification-status-text error-text">
                    {t("notifications.uploadFailed")}
                  </span>{" "}
                  <div className="notification-progress-bar-outer">
                    <div
                      className="notification-progress-bar-inner"
                      style={{ width: `100%` }}
                    />
                  </div>{" "}
                  {n.message && (
                    <p className="notification-error-message">{n.message}</p>
                  )}{" "}
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
          {" "}
          <button
            className="files-modal-close"
            onClick={toggleFilesModal}
            aria-label="Close files modal"
          >
            &times;
          </button>{" "}
          <iframe src="./files/" title="Downloadable Files" />{" "}
        </div>
      )}
      {isAppsModalOpen && (
        <AppsModal isOpen={isAppsModalOpen} onClose={toggleAppsModal} t={t} />
      )}

      {(isMobile || hasDetectedTouch) && isKeyboardButtonVisible && (
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
