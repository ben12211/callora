import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { performance } from 'node:perf_hooks';
import path from 'node:path';
import { URL } from 'node:url';
import WebSocket from 'ws';
import { HebrewSpeechFrontend, PronunciationCache } from '../dist/hebrew/speech-frontend.js';
import { ReNikudClient } from '../dist/hebrew/renikud-client.js';
import { renderDeepdubText } from '../dist/realtime/deepdub-connection.js';

const required = ['DEEPDUB_API_KEY', 'DEEPDUB_VOICE_ID'];
for (const name of required) if (!process.env[name]) throw new Error(`${name} is required`);

const dataset = JSON.parse(await readFile(new URL('../evaluation/hebrew-utterances.json', import.meta.url), 'utf8'));
const utterances = Object.entries(dataset).flatMap(([category, values]) => values.map((text) => ({ category, text })));
const sampleCount = Math.min(Number(process.env.BENCHMARK_SAMPLES ?? 20), utterances.length);
const selected = utterances.slice(0, sampleCount);
const modes = (process.env.BENCHMARK_MODES ?? 'off,smart,strict').split(',');
const outputDir = process.env.HEBREW_EVAL_OUTPUT_DIR;
const dictionary = process.env.PRONUNCIATION_DICTIONARY_JSON
  ? JSON.parse(await readFile(process.env.PRONUNCIATION_DICTIONARY_JSON, 'utf8'))
  : [];

const percentile = (values, p) => {
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.min(sorted.length - 1, Math.ceil(sorted.length * p) - 1)] ?? 0;
};
const summary = (values) => ({ median: percentile(values, 0.5), p95: percentile(values, 0.95) });

async function connect() {
  const ws = new WebSocket(process.env.DEEPDUB_WS_URL ?? 'wss://wss.deepdub.ai/ws', {
    headers: { 'x-api-key': process.env.DEEPDUB_API_KEY },
  });
  await new Promise((resolve, reject) => {
    ws.once('error', reject);
    ws.on('message', function hello(data) {
      const message = JSON.parse(data.toString());
      if (message.action === 'status' && message.message === 'connected') {
        ws.off('message', hello);
        resolve();
      }
    });
  });
  ws.send(JSON.stringify({ action: 'stream-config', data: {
    model: process.env.DEEPDUB_MODEL ?? 'dd-etts-3.0',
    voicePromptId: process.env.DEEPDUB_VOICE_ID,
    locale: process.env.DEEPDUB_LOCALE ?? 'he-IL',
    format: 'mulaw', sampleRate: 8000, realtime: true, cleanAudio: true,
    enableLogging: process.env.DEEPDUB_ENABLE_LOGGING === 'true', firstAudioTimeout: 50,
  } }));
  return ws;
}

async function synthesize(ws, text) {
  return await new Promise((resolve, reject) => {
    const chunks = [];
    const started = performance.now();
    let firstAudioMs = null;
    const handler = (data) => {
      const message = JSON.parse(data.toString());
      if (message.action === 'error' || message.error) {
        ws.off('message', handler);
        reject(new Error(message.message ?? message.error));
        return;
      }
      if (message.data) {
        if (firstAudioMs === null) firstAudioMs = performance.now() - started;
        chunks.push(Buffer.from(message.data, 'base64'));
      }
      if (message.isFinished && message.isFinal) {
        ws.off('message', handler);
        resolve({ firstAudioMs: firstAudioMs ?? performance.now() - started, audio: Buffer.concat(chunks) });
      }
    };
    ws.on('message', handler);
    ws.send(JSON.stringify({ action: 'stream-text', data: { text } }));
    ws.send(JSON.stringify({ action: 'end-stream' }));
  });
}

const ws = await connect();
const report = {};
try {
  if (outputDir) await mkdir(outputDir, { recursive: true });
  for (const mode of modes) {
    const renikud = process.env.RENIKUD_URL ? new ReNikudClient({ baseUrl: process.env.RENIKUD_URL, timeoutMs: Number(process.env.RENIKUD_TIMEOUT_MS ?? 90) }) : undefined;
    const frontend = new HebrewSpeechFrontend({ businessId: 'benchmark', locale: 'he-IL', mode, dictionary, cache: new PronunciationCache(), ...(renikud ? { renikud } : {}) });
    const preprocessing = [];
    const provider = [];
    for (let index = 0; index < selected.length; index += 1) {
      const item = selected[index];
      const preparation = await frontend.preprocess(item.text);
      preprocessing.push(preparation.processingMs);
      const result = await synthesize(ws, renderDeepdubText(preparation));
      provider.push(result.firstAudioMs);
      if (outputDir) await writeFile(path.join(outputDir, `${String(index + 1).padStart(3, '0')}-${mode}.ulaw`), result.audio);
    }
    report[mode] = { samples: selected.length, pronunciationMs: summary(preprocessing), providerTtfaMs: summary(provider), calloraAddedPlusProviderMs: summary(provider.map((value, index) => value + preprocessing[index])) };
  }
} finally {
  ws.close();
}
console.log(JSON.stringify(report, null, 2));
