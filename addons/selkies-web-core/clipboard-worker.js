// clipboard-worker.js

// 32KB chunk size
const CHUNK_SIZE = 0x8000;

// Converts given string to base64 encoded string with UTF-8 format
function stringToBase64(text) {
    const bytes = new TextEncoder().encode(text);
    let binString = "";
    for (let i = 0; i < bytes.length; i += CHUNK_SIZE) {
        const chunk = bytes.subarray(i, i + CHUNK_SIZE);
        binString += String.fromCharCode.apply(null, chunk);
    }
    return btoa(binString);
}

// Converts given base64 UTF-8 format encoded string to its original form
function base64ToString(base64) {
    const binString = atob(base64);
    const len = binString.length;
    const bytes = new Uint8Array(len);
    for (let i = 0; i < len; i++) {
        bytes[i] = binString.charCodeAt(i);
    }
    return { text: new TextDecoder().decode(bytes), byteLength: len };
}

// Converts base64 encoded string to bytes
function base64ToBytes(base64) {
    const binString = atob(base64);
    const len = binString.length;
    const bytes = new Uint8Array(len);
    for (let i = 0; i < len; i++) {
        bytes[i] = binString.charCodeAt(i);
    }
    return bytes;
}

// Converts bytes to base64 encoded string
function bytesToBase64(bytes) {
    let binString = "";
    for (let i = 0; i < bytes.length; i += CHUNK_SIZE) {
        const chunk = bytes.subarray(i, i + CHUNK_SIZE);
        binString += String.fromCharCode.apply(null, chunk);
    }
    return btoa(binString);
}


// Worker Message Handler
self.onmessage = function(e) {
    const { id, action, payload, mimeType } = e.data;

    try {
        if (action === 'ENCODE_BINARY_TO_B64') {
            const bytes = new Uint8Array(payload);
            const base64 = bytesToBase64(bytes);
            self.postMessage({ id, success: true, result: base64 });
        } 
        else if (action === 'ENCODE_TEXT_TO_B64') {
            // payload is a standard string
            const base64 = stringToBase64(payload);
            self.postMessage({ id, success: true, result: base64 });
        }
        else if (action === 'DECODE_FROM_B64') {
            if (mimeType === 'text/plain') {
                const { text, byteLength } = base64ToString(payload);
                self.postMessage({ id, success: true, result: text, mimeType, byteLength });
            } else {
                const bytes = base64ToBytes(payload);
                self.postMessage(
                    { id, success: true, result: bytes.buffer, mimeType, byteLength: bytes.byteLength }, 
                    [bytes.buffer] 
                );
            }
        } else {
            self.postMessage({ id, success: false, error: `Unknown action: ${action}` });
        }
    } catch (err) {
        self.postMessage({ id, success: false, error: err.message });
    }
};