const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { test } = require('node:test');
const vm = require('node:vm');
const ts = require('typescript');

// Execute the actual transport with deterministic browser/network doubles. In
// particular, finish gathering and fire the last timer BEFORE delivering WHEP.
function setup(patch = async () => {}) {
    const timers = new Map();
    const requests = [];
    let nextTimer = 0;
    class Peer {
        iceGatheringState = 'gathering';
        listeners = new Map();
        addEventListener(name, handler) { this.listeners.set(name, handler); }
        createDataChannel() { return { bufferedAmount: 0, readyState: 'connecting' }; }
        addTransceiver() {}
        async createOffer() { return { type: 'offer', sdp: 'offer' }; }
        async setLocalDescription() {}
        async setRemoteDescription() {}
        close() {}
        candidate(value) { this.listeners.get('icecandidate')({ candidate: { toJSON: () => ({ candidate: value }) } }); }
        complete() { this.iceGatheringState = 'complete'; this.listeners.get('icecandidate')({ candidate: null }); }
    }
    const environment = {
        setTimeout(fn) { const id = ++nextTimer; timers.set(id, fn); return id; },
        clearTimeout(id) { timers.delete(id); },
        requestAnimationFrame() { return 0; },
    };
    const bindings = {
        webrtcSessionOfferApply: sdp => sdp,
        webrtcSessionAnswerParse: () => ({}),
    };
    const exports = {};
    const context = vm.createContext({
        exports, RTCPeerConnection: Peer, console: { debug() {} },
        require(name) {
            if (name === '../../api') return { fetchApi: async (_api, url, method, options) => {
                requests.push({ url, method, body: options.trickleIceSdpFrag });
                if (method === 'PATCH') await patch();
            } };
            if (name === '../../util') return { globalObject: () => environment };
            if (name === '../clipboard') return { ClipboardChannel: class {} };
            if (name === '../../uniffi/moonlight_common_bindings') return bindings;
            return {};
        },
    });
    const source = readFileSync(new URL('../web/stream/transport/webrtc.ts', 'file://' + __filename), 'utf8');
    vm.runInContext(ts.transpileModule(source, {
        compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.CommonJS },
    }).outputText, context);
    const transport = new exports.WebRTCTransport({}, {});
    return {
        transport, peer: transport.peer, requests, timers,
        async tick() {
            const pending = [...timers.values()]; timers.clear();
            for (const fn of pending) await fn();
        },
    };
}

const answer = { location: '/api/host/stream/webrtc/42', answerSdp: 'answer' };

test('flushes gathered candidates when a delayed WHEP answer supplies Location', async () => {
    const s = setup();
    await s.transport.createOffer({});
    s.peer.candidate('candidate:host 1 udp 1 browser.local 50000 typ host');
    s.peer.candidate('candidate:stun 1 udp 1 192.168.33.30 50000 typ srflx');
    s.peer.complete();
    await s.tick();
    assert.equal(s.requests.length, 0);
    assert.equal(s.timers.size, 0, 'the gather timer has already stopped');
    await s.transport.setAnswer(answer);
    assert.equal(s.requests.length, 1);
    assert.equal(s.requests[0].method, 'PATCH');
    assert.equal(s.requests[0].url, answer.location);
    assert.equal(s.requests[0].body, 'a=candidate:host 1 udp 1 browser.local 50000 typ host\r\na=candidate:stun 1 udp 1 192.168.33.30 50000 typ srflx');
});

test('retains candidates arriving while a PATCH is in flight', async () => {
    let complete;
    const s = setup(() => new Promise(resolve => { complete = resolve; }));
    await s.transport.createOffer({});
    s.peer.candidate('first');
    await s.transport.setAnswer(answer);
    s.peer.candidate('second'); s.peer.complete();
    complete();
    await new Promise(setImmediate);
    const tick = s.tick();
    assert.equal(s.requests[1].body, 'a=second');
    complete(); await tick;
    assert.equal(s.timers.size, 0);
});

test('retries a failed batch after gathering has completed', async () => {
    let attempts = 0;
    const s = setup(async () => { if (++attempts === 1) throw new Error('temporary failure'); });
    await s.transport.createOffer({});
    s.peer.candidate('retry-me'); s.peer.complete();
    await s.transport.setAnswer(answer);
    await new Promise(setImmediate);
    await s.tick();
    assert.equal(s.requests.length, 2);
    assert.equal(s.requests[1].body, 'a=retry-me');
    assert.equal(s.timers.size, 0);
});

test('closing while a PATCH is in flight does not restart the timer', async () => {
    let complete;
    const s = setup(() => new Promise(resolve => { complete = resolve; }));
    await s.transport.createOffer({}); s.peer.candidate('first');
    await s.transport.setAnswer(answer);
    await s.transport.close();
    complete(); await new Promise(setImmediate);
    assert.equal(s.timers.size, 0);
});
