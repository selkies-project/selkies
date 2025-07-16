/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Microphone capture and transmission for bidirectional audio
 * Handles getUserMedia permissions, audio processing, and WebRTC transmission
 */
class MicrophoneManager {
    constructor(webrtcDemo) {
        this.webrtcDemo = webrtcDemo;
        this.audioContext = null;
        this.mediaStream = null;
        this.sourceNode = null;
        this.processorNode = null;
        this.peerConnection = null;
        this.dataChannel = null;
        this.isEnabled = false;
        this.isTransmitting = false;
        
        // Audio processing settings
        this.sampleRate = 48000;
        this.channels = 1; // Mono for microphone
        this.bufferSize = 4096;
        this.silenceThreshold = 0.01; // Threshold for silence detection
        
        // Opus encoding simulation (would need real opus encoder)
        this.audioBuffer = [];
        this.packetDuration = 10; // 10ms packets as mentioned in requirements
        
        this.onstatuschange = null;
        this.onerror = null;
        this.ondebug = null;
    }

    /**
     * Request microphone permissions and initialize audio context
     */
    async requestPermissions() {
        try {
            this._setStatus("Requesting microphone permissions...");
            
            const constraints = {
                audio: {
                    channelCount: this.channels,
                    sampleRate: this.sampleRate,
                    echoCancellation: true,
                    noiseSuppression: true,
                    autoGainControl: true
                },
                video: false
            };

            this.mediaStream = await navigator.mediaDevices.getUserMedia(constraints);
            this._setStatus("Microphone permissions granted");
            this._setDebug(`Microphone stream acquired: ${this.mediaStream.getAudioTracks().length} audio tracks`);
            
            return true;
        } catch (error) {
            this._setError(`Failed to get microphone permissions: ${error.message}`);
            return false;
        }
    }

    /**
     * Initialize audio context and processing nodes
     */
    async initializeAudioProcessing() {
        try {
            if (!this.mediaStream) {
                throw new Error("No media stream available. Call requestPermissions() first.");
            }

            // Create audio context
            this.audioContext = new (window.AudioContext || window.webkitAudioContext)({
                sampleRate: this.sampleRate
            });

            // Create source node from microphone stream
            this.sourceNode = this.audioContext.createMediaStreamSource(this.mediaStream);
            
            // Create processor node for audio processing
            this.processorNode = this.audioContext.createScriptProcessor(this.bufferSize, this.channels, this.channels);
            this.processorNode.onaudioprocess = this._processAudioData.bind(this);

            // Connect nodes
            this.sourceNode.connect(this.processorNode);
            this.processorNode.connect(this.audioContext.destination);

            this._setStatus("Audio processing initialized");
            this._setDebug(`Audio context: ${this.audioContext.sampleRate}Hz, ${this.channels} channels`);
            
            return true;
        } catch (error) {
            this._setError(`Failed to initialize audio processing: ${error.message}`);
            return false;
        }
    }

    /**
     * Set up WebRTC peer connection for microphone transmission
     */
    async setupWebRTCConnection() {
        try {
            // Create separate peer connection for microphone (following existing dual-connection pattern)
            this.peerConnection = new RTCPeerConnection(this.webrtcDemo.rtcPeerConfig);
            
            // Add microphone track to peer connection
            if (this.mediaStream) {
                const audioTrack = this.mediaStream.getAudioTracks()[0];
                this.peerConnection.addTrack(audioTrack, this.mediaStream);
                this._setDebug("Added microphone track to peer connection");
            }

            // Create data channel for microphone control
            this.dataChannel = this.peerConnection.createDataChannel("microphone", {
                ordered: true
            });
            
            this.dataChannel.onopen = () => {
                this._setStatus("Microphone data channel opened");
            };
            
            this.dataChannel.onmessage = (event) => {
                this._handleDataChannelMessage(event);
            };

            // Set up ICE candidate handling
            this.peerConnection.onicecandidate = (event) => {
                if (event.candidate) {
                    // Send ICE candidate to signaling server (extend existing signaling)
                    this._setDebug("Generated microphone ICE candidate");
                }
            };

            this._setStatus("WebRTC connection for microphone initialized");
            return true;
        } catch (error) {
            this._setError(`Failed to setup WebRTC connection: ${error.message}`);
            return false;
        }
    }

    /**
     * Process audio data from microphone
     * Implements silence detection and 16-bit integer encoding as suggested
     */
    _processAudioData(audioProcessingEvent) {
        const inputBuffer = audioProcessingEvent.inputBuffer;
        const inputData = inputBuffer.getChannelData(0); // Get mono channel
        
        // Convert float32 to signed 16-bit integers
        const int16Array = new Int16Array(inputData.length);
        let hasAudio = false;
        
        for (let i = 0; i < inputData.length; i++) {
            // Convert float32 (-1.0 to 1.0) to int16 (-32768 to 32767)
            const sample = Math.max(-1, Math.min(1, inputData[i]));
            int16Array[i] = sample < 0 ? sample * 0x8000 : sample * 0x7FFF;
            
            // Check for silence (all zeros or below threshold)
            if (Math.abs(sample) > this.silenceThreshold) {
                hasAudio = true;
            }
        }

        // Skip transmission if all zeros (silence detection)
        if (!hasAudio) {
            return;
        }

        // Buffer audio data for packet transmission
        this.audioBuffer.push(int16Array);
        
        // Check if we have enough data for a packet (10ms worth)
        const samplesPerPacket = (this.sampleRate * this.packetDuration) / 1000;
        const totalSamples = this.audioBuffer.reduce((sum, buffer) => sum + buffer.length, 0);
        
        if (totalSamples >= samplesPerPacket) {
            this._transmitAudioPacket();
        }
    }

    /**
     * Transmit audio packet via WebRTC data channel
     * In real implementation, this would encode to Opus first
     */
    _transmitAudioPacket() {
        if (!this.isTransmitting || !this.dataChannel || this.dataChannel.readyState !== 'open') {
            return;
        }

        try {
            // Combine buffered audio data
            const totalSamples = this.audioBuffer.reduce((sum, buffer) => sum + buffer.length, 0);
            const combinedBuffer = new Int16Array(totalSamples);
            
            let offset = 0;
            for (const buffer of this.audioBuffer) {
                combinedBuffer.set(buffer, offset);
                offset += buffer.length;
            }

            // In real implementation, encode to Opus here
            // For now, send raw PCM data with metadata
            const audioPacket = {
                type: 'microphone_audio',
                data: {
                    pcm_data: Array.from(combinedBuffer), // Convert to regular array for JSON
                    sample_rate: this.sampleRate,
                    channels: this.channels,
                    timestamp: Date.now()
                }
            };

            this.dataChannel.send(JSON.stringify(audioPacket));
            this._setDebug(`Transmitted microphone packet: ${combinedBuffer.length} samples`);
            
            // Clear buffer
            this.audioBuffer = [];
            
        } catch (error) {
            this._setError(`Failed to transmit audio packet: ${error.message}`);
        }
    }

    /**
     * Handle incoming data channel messages
     */
    _handleDataChannelMessage(event) {
        try {
            const message = JSON.parse(event.data);
            
            switch (message.type) {
                case 'microphone_control':
                    this._handleMicrophoneControl(message.data);
                    break;
                case 'microphone_status':
                    this._setStatus(`Server: ${message.data.status}`);
                    break;
                default:
                    this._setDebug(`Unknown microphone message: ${message.type}`);
            }
        } catch (error) {
            this._setError(`Failed to parse microphone data channel message: ${error.message}`);
        }
    }

    /**
     * Handle microphone control messages from server
     */
    _handleMicrophoneControl(data) {
        switch (data.action) {
            case 'start':
                this.startTransmission();
                break;
            case 'stop':
                this.stopTransmission();
                break;
            case 'mute':
                this.mute();
                break;
            case 'unmute':
                this.unmute();
                break;
            default:
                this._setDebug(`Unknown microphone control action: ${data.action}`);
        }
    }

    /**
     * Start microphone transmission
     */
    async startTransmission() {
        try {
            if (!this.mediaStream) {
                const hasPermissions = await this.requestPermissions();
                if (!hasPermissions) return false;
            }

            if (!this.audioContext) {
                const initialized = await this.initializeAudioProcessing();
                if (!initialized) return false;
            }

            if (!this.peerConnection) {
                const connected = await this.setupWebRTCConnection();
                if (!connected) return false;
            }

            this.isTransmitting = true;
            this._setStatus("Microphone transmission started");
            this._setDebug("Audio processing and transmission active");
            
            return true;
        } catch (error) {
            this._setError(`Failed to start microphone transmission: ${error.message}`);
            return false;
        }
    }

    /**
     * Stop microphone transmission
     */
    stopTransmission() {
        this.isTransmitting = false;
        this._setStatus("Microphone transmission stopped");
    }

    /**
     * Mute microphone (stop processing but keep connection)
     */
    mute() {
        if (this.mediaStream) {
            this.mediaStream.getAudioTracks().forEach(track => {
                track.enabled = false;
            });
        }
        this._setStatus("Microphone muted");
    }

    /**
     * Unmute microphone
     */
    unmute() {
        if (this.mediaStream) {
            this.mediaStream.getAudioTracks().forEach(track => {
                track.enabled = true;
            });
        }
        this._setStatus("Microphone unmuted");
    }

    /**
     * Enable microphone functionality
     */
    async enable() {
        this.isEnabled = true;
        return await this.startTransmission();
    }

    /**
     * Disable and cleanup microphone functionality
     */
    disable() {
        this.isEnabled = false;
        this.stopTransmission();
        
        // Cleanup resources
        if (this.processorNode) {
            this.processorNode.disconnect();
            this.processorNode = null;
        }
        
        if (this.sourceNode) {
            this.sourceNode.disconnect();
            this.sourceNode = null;
        }
        
        if (this.audioContext) {
            this.audioContext.close();
            this.audioContext = null;
        }
        
        if (this.mediaStream) {
            this.mediaStream.getTracks().forEach(track => track.stop());
            this.mediaStream = null;
        }
        
        if (this.peerConnection) {
            this.peerConnection.close();
            this.peerConnection = null;
        }
        
        this._setStatus("Microphone disabled and cleaned up");
    }

    /**
     * Check if microphone is supported
     */
    static isSupported() {
        return !!(navigator.mediaDevices && navigator.mediaDevices.getUserMedia);
    }

    /**
     * Get available audio input devices
     */
    static async getAudioInputDevices() {
        try {
            const devices = await navigator.mediaDevices.enumerateDevices();
            return devices.filter(device => device.kind === 'audioinput');
        } catch (error) {
            console.error('Failed to enumerate audio devices:', error);
            return [];
        }
    }

    // Status and debug methods
    _setStatus(message) {
        if (this.onstatuschange) {
            this.onstatuschange(`[Microphone] ${message}`);
        }
    }

    _setDebug(message) {
        if (this.ondebug) {
            this.ondebug(`[Microphone] ${message}`);
        }
    }

    _setError(message) {
        if (this.onerror) {
            this.onerror(`[Microphone] ${message}`);
        }
    }
}

// Export for use in other modules
if (typeof module !== 'undefined' && module.exports) {
    module.exports = MicrophoneManager;
}