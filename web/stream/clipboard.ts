import { showNotification } from "../component/notification"

const MAX_BYTES = 1024 * 1024
const encoder = new TextEncoder()

/** Reliable clipboard messages, independent of Moonlight keyboard input. */
export class ClipboardChannel {
    private channel: RTCDataChannel
    private sequence = 0
    private pending: { id: number, resolve: () => void, reject: (e: Error) => void, timer: number } | null = null
    private incoming: { id: number, bytes: number, text: string, received: number, initial: boolean } | null = null
    private remote: string | null = null
    private warned = false

    constructor(peer: RTCPeerConnection) {
        this.channel = peer.createDataChannel("moonlight.clipboard.v1", { ordered: true })
        this.channel.addEventListener("message", event => this.receive(event.data))
        this.channel.addEventListener("close", () => {
            this.fail("Clipboard channel disconnected")
            this.incoming = null
            this.remote = null
        })
    }

    private fail(message: string) {
        if (this.pending) {
            window.clearTimeout(this.pending.timer)
            this.pending.reject(new Error(message))
            this.pending = null
        }
    }

    private receive(data: unknown) {
        try {
            if (typeof data !== "string" || data.length > 32768) throw new Error("Invalid clipboard message")
            const message = JSON.parse(data)
            switch (message.type) {
                case "ack":
                    if (this.pending && this.pending.id === message.id) {
                        window.clearTimeout(this.pending.timer)
                        this.pending.resolve()
                        this.pending = null
                    }
                    break
                case "unavailable":
                    this.remote = null
                    break
                case "error":
                    this.fail(message.message)
                    if (!this.warned) {
                        showNotification(message.message, "warn")
                        this.warned = true
                    }
                    break
                case "begin":
                    if (!Number.isInteger(message.bytes) || message.bytes < 0 || message.bytes > MAX_BYTES) throw new Error("Clipboard exceeds 1 MiB")
                    this.incoming = { id: message.id, bytes: message.bytes, text: "", received: 0, initial: message.initial === true }
                    break
                case "chunk": {
                    const transfer = this.incoming
                    if (!transfer || transfer.id !== message.id || typeof message.text !== "string") throw new Error("Invalid clipboard transfer")
                    transfer.received += encoder.encode(message.text).length
                    if (transfer.received > transfer.bytes) throw new Error("Clipboard exceeds declared size")
                    transfer.text += message.text
                    break
                }
                case "end": {
                    const transfer = this.incoming
                    this.incoming = null
                    if (!transfer || transfer.id !== message.id || transfer.received !== transfer.bytes) throw new Error("Incomplete clipboard transfer")
                    this.remote = transfer.text
                    // Never overwrite a local clipboard while a paste is in flight.
                    if (!transfer.initial && !this.pending && document.hasFocus()) void this.copyRemote(false)
                    break
                }
            }
        } catch {
            this.incoming = null
            this.fail("Invalid clipboard response")
        }
    }

    /** Also serves as a user-gesture fallback on browsers that deny background writes. */
    async copyRemote(notify = true) {
        if (this.remote === null) {
            if (notify) showNotification("No remote text clipboard received yet", "warn")
            return
        }
        try {
            await navigator.clipboard.writeText(this.remote)
            this.warned = false
            if (notify) showNotification("Remote clipboard copied")
        } catch {
            if (!this.warned || notify) showNotification("Allow clipboard access, or use Copy remote clipboard in the sidebar", "warn")
            this.warned = true
        }
    }

    requestRemote() {
        // Copy/cut may produce the same text as last time. Refresh it explicitly,
        // allowing the remote application to process the keyboard shortcut.
        for (const delay of [150, 500, 1000]) window.setTimeout(() => {
            if (this.channel.readyState === "open") this.channel.send(JSON.stringify({ type: "read" }))
        }, delay)
    }

    async setText(text: string): Promise<void> {
        if (this.channel.readyState !== "open") throw new Error("Clipboard sharing requires an active WebRTC stream")
        if (this.pending) throw new Error("A clipboard transfer is already in progress")
        const bytes = encoder.encode(text).length
        if (bytes > MAX_BYTES || text.includes("\0")) throw new Error("Clipboard must be text without NUL, at most 1 MiB")
        const id = ++this.sequence
        const result = new Promise<void>((resolve, reject) => {
            this.pending = { id, resolve, reject, timer: window.setTimeout(() => this.fail("Clipboard transfer timed out"), 15000) }
        })
        // Register rejection handling before waiting for data-channel backpressure.
        void result.catch(() => {})
        try {
            this.channel.send(JSON.stringify({ type: "begin", id, bytes }))
            let chunk = ""
            for (const char of text) {
                chunk += char
                if (chunk.length >= 2048) {
                    await this.sendChunk(id, chunk)
                    chunk = ""
                }
            }
            if (chunk) await this.sendChunk(id, chunk)
            if (!this.isPending(id)) throw new Error("Clipboard transfer cancelled")
            this.channel.send(JSON.stringify({ type: "end", id }))
        } catch (error) {
            this.fail(error instanceof Error ? error.message : "Clipboard transfer failed")
        }
        return result
    }

    private isPending(id: number): boolean { return this.pending?.id === id }

    private async sendChunk(id: number, text: string) {
        const deadline = performance.now() + 5000
        while (this.channel.bufferedAmount > 65536) {
            if (performance.now() > deadline || this.channel.readyState !== "open") throw new Error("Clipboard channel is stalled")
            await new Promise(resolve => window.setTimeout(resolve, 10))
        }
        if (!this.isPending(id)) throw new Error("Clipboard transfer cancelled")
        this.channel.send(JSON.stringify({ type: "chunk", id, text }))
    }
}
