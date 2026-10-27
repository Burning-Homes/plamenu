// Standalone environment diagnostic: no Plamenu server, accounts or gate result.
// Run inside the pinned browser image; --headed also compares GTK under Xvfb.
import { chromium, webkit } from '@playwright/test';
import { createServer } from 'node:http';
import { mkdir, writeFile, readFile } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import path from 'node:path';
import setupAudio from './audio-setup.mjs';

const args = process.argv.slice(2);
const headed = args.includes('--headed');
const selection = args.find(arg => arg.startsWith('--case='))?.slice(7);
const cases = [
  { name: 'chromium-headless-control', engine: chromium },
  { name: 'webkit-headless-desktop', engine: webkit },
  { name: 'webkit-headless-mobile', engine: webkit, isMobile: true },
  ...(headed ? [{ name: 'webkit-headed-desktop', engine: webkit, headless: false }] : []),
];
const output = args.filter(arg => !arg.startsWith('--'));
if (output.length !== 1 || args.some(arg => arg.startsWith('--') && arg !== '--headed' && !arg.startsWith('--case='))
    || (selection && !cases.some(config => config.name === selection))) {
  throw new Error('Usage: node accessibility/probe-media-runtime.mjs OUTPUT_DIRECTORY [--headed] [--case=NAME]');
}
const directory = path.resolve(output[0]);
// Never overwrite a completed diagnostic.
await mkdir(directory);
const report = {
  kind: 'browser-runtime-diagnostic', full_pass: false, status: 'running',
  started_at: new Date().toISOString(), platform: process.platform,
  image: process.env.A11Y_BROWSER_IMAGE || null,
  playwright: JSON.parse(await readFile(new URL('./node_modules/@playwright/test/package.json', import.meta.url))).version,
  probe_sha256: createHash('sha256').update(await readFile(new URL(import.meta.url))).digest('hex'),
  cases: [],
};
const server = createServer((_request, response) => {
  response.writeHead(200, { 'Content-Type': 'text/html' });
  response.end('<!doctype html><title>Media runtime diagnostic</title><button id="record">Record</button><button id="fullscreen">Fullscreen</button>');
});
let closeAudio;
try {
  closeAudio = await setupAudio({ workers: 1 });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const origin = `http://127.0.0.1:${server.address().port}`;
  for (const config of cases.filter(config => !selection || config.name === selection)) {
    const result = { name: config.name, recording: 'not-run', fullscreen: 'not-run' };
    report.cases.push(result);
    let browser;
    try {
      browser = await config.engine.launch({ headless: config.headless ?? true, timeout: 30_000 });
      result.browser_version = browser.version();
      const context = await browser.newContext({
        viewport: config.isMobile ? { width: 320, height: 800 } : { width: 1440, height: 900 },
        isMobile: Boolean(config.isMobile), hasTouch: Boolean(config.isMobile),
      });
      const page = await context.newPage();
      page.setDefaultTimeout(15_000);
      await page.goto(origin);
      result.capabilities = await page.evaluate(() => ({
        secure: isSecureContext, recorder: typeof MediaRecorder,
        canvas_capture: typeof HTMLCanvasElement.prototype.captureStream,
        audio_context: typeof AudioContext,
        fullscreen_enabled: Boolean(document.fullscreenEnabled),
        request_fullscreen: typeof document.documentElement.requestFullscreen,
        mime_types: typeof MediaRecorder === 'function' ? Object.fromEntries([
          'video/webm;codecs=vp8,opus', 'video/webm;codecs=vp9,opus',
          'video/mp4;codecs=avc1.42E01E,mp4a.40.2', 'video/mp4',
        ].map(type => [type, MediaRecorder.isTypeSupported(type)])) : {},
      }));
      if (!result.capabilities.secure) throw new Error('Probe requires a trustworthy origin');
      if (result.capabilities.recorder !== 'function') {
        result.recording = 'unsupported';
      } else {
        result.recording = 'in-progress';
        await page.evaluate(() => {
          document.querySelector('#record').onclick = async () => {
            let audio, stream, timer, recorder;
            try {
              const canvas = document.createElement('canvas');
              canvas.width = 320; canvas.height = 180;
              document.body.append(canvas);
              const drawing = canvas.getContext('2d');
              let frame = 0;
              const paint = () => {
                drawing.fillStyle = frame++ % 2 ? '#11bb55' : '#315fdb';
                drawing.fillRect(0, 0, canvas.width, canvas.height);
              };
              paint(); timer = setInterval(paint, 50);
              audio = new AudioContext();
              const oscillator = audio.createOscillator();
              const destination = audio.createMediaStreamDestination();
              oscillator.frequency.value = 440;
              oscillator.connect(destination); oscillator.start();
              await audio.resume();
              stream = new MediaStream([
                ...canvas.captureStream(20).getTracks(), ...destination.stream.getTracks(),
              ]);
              const mimeType = [
                'video/webm;codecs=vp8,opus', 'video/webm',
                'video/mp4;codecs=avc1.42E01E,mp4a.40.2', 'video/mp4',
              ].find(type => MediaRecorder.isTypeSupported(type));
              if (!mimeType) throw new Error('No supported recording format');
              recorder = new MediaRecorder(stream, { mimeType });
              const chunks = [];
              const stopped = new Promise((resolve, reject) => {
                recorder.ondataavailable = event => { if (event.data.size) chunks.push(event.data); };
                recorder.onerror = event => reject(event.error || new Error('Recording failed'));
                recorder.onstop = resolve;
              });
              recorder.start(250);
              // Measure actual media time: a cold audio backend can take longer
              // than a short wall-clock sample to start producing any frames.
              const started = audio.currentTime;
              const deadline = performance.now() + 10_000;
              while (audio.currentTime - started < 1.5) {
                if (performance.now() >= deadline) throw new Error('Audio clock did not advance by 1.5 seconds');
                await new Promise(resolve => setTimeout(resolve, 50));
              }
              recorder.stop(); await stopped;
              const blob = new Blob(chunks, { type: recorder.mimeType });
              const bytes = new Uint8Array(await blob.arrayBuffer());
              window.probeRecording = {
                mime_type: recorder.mimeType, bytes: Array.from(bytes),
                capture: { frames: frame, audio_state: audio.state, audio_time: audio.currentTime,
                  tracks: stream.getTracks().map(track => ({ kind: track.kind, state: track.readyState, muted: track.muted })) },
              };
            } catch (error) {
              window.probeRecording = { error: String(error) };
            } finally {
              clearInterval(timer);
              if (recorder?.state === 'recording') recorder.stop();
              stream?.getTracks().forEach(track => track.stop());
              await audio?.close();
            }
          };
        });
        await page.locator('#record').click();
        await page.waitForFunction(() => window.probeRecording !== undefined);
        const recorded = await page.evaluate(() => window.probeRecording);
        if (recorded.error) throw new Error(recorded.error);
        result.capture = recorded.capture;
        if (!recorded.bytes.length) throw new Error('Recorder produced no bytes');
        const file = path.join(directory, `${config.name}.media`);
        await writeFile(file, Buffer.from(recorded.bytes));
        const probe = JSON.parse(execFileSync('ffprobe', [
          '-v', 'error', '-show_streams', '-of', 'json', file,
        ], { timeout: 15_000, encoding: 'utf8' }));
        for (const type of ['audio', 'video']) {
          if (!probe.streams.some(stream => stream.codec_type === type)) throw new Error(`Missing ${type} stream`);
        }
        execFileSync('ffmpeg', [
          '-v', 'error', '-xerror', '-i', file, '-map', '0:v:0', '-map', '0:a:0', '-f', 'null', '-',
        ], { timeout: 15_000, stdio: 'pipe' });
        result.recording = 'encoded-and-decoded';
        result.bytes = recorded.bytes.length;
        result.mime_type = recorded.mime_type;
        result.streams = probe.streams;
      }
      if (result.capabilities.fullscreen_enabled && result.capabilities.request_fullscreen === 'function') {
        result.fullscreen = 'in-progress';
        await page.evaluate(() => {
          document.querySelector('#fullscreen').onclick = () => {
            document.documentElement.requestFullscreen().catch(error => { window.probeFullscreenError = String(error); });
          };
        });
        await page.locator('#fullscreen').click();
        await page.waitForFunction(() => document.fullscreenElement || window.probeFullscreenError);
        const error = await page.evaluate(() => window.probeFullscreenError);
        if (error) throw new Error(error);
        await page.evaluate(() => document.exitFullscreen());
        await page.waitForFunction(() => !document.fullscreenElement);
        result.fullscreen = 'entered-and-exited';
      } else result.fullscreen = 'unsupported';
    } catch (error) {
      for (const operation of ['recording', 'fullscreen']) {
        if (result[operation] === 'in-progress') result[operation] = 'failed';
      }
      result.error = String(error);
    } finally {
      await browser?.close();
      await writeFile(path.join(directory, 'report.json'), JSON.stringify(report, null, 2) + '\n');
      console.log(JSON.stringify(result));
    }
  }
  report.status = report.cases.some(item => item.error) ? 'diagnostic-errors' : 'complete';
  if (report.status !== 'complete') process.exitCode = 1;
} catch (error) {
  report.status = 'diagnostic-errors';
  report.error = String(error);
  process.exitCode = 1;
} finally {
  await new Promise(resolve => server.close(resolve));
  await closeAudio?.();
  report.finished_at = new Date().toISOString();
  await writeFile(path.join(directory, 'report.json'), JSON.stringify(report, null, 2) + '\n');
}
