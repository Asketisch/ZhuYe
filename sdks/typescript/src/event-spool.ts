import { closeSync, mkdtempSync, openSync, readSync, rmSync, writeSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { Event } from './protocol.js';

const activeSpools = new Set<EventSpool>();
function cleanupSpools(): void {
  for (const spool of activeSpools) spool.clear();
}

/** 保序存储未消费事件,队列排空时删除临时文件。 */
export class EventSpool {
  private fd: number | undefined;
  private dir: string | undefined;
  private readOffset = 0;
  private writeOffset = 0;

  get empty(): boolean { return this.readOffset === this.writeOffset; }

  push(event: Event): void {
    if (this.fd === undefined) {
      this.dir = mkdtempSync(join(tmpdir(), 'zhuye-events-'));
      try {
        this.fd = openSync(join(this.dir, 'events'), 'wx+', 0o600);
        if (activeSpools.size === 0) process.once('exit', cleanupSpools);
        activeSpools.add(this);
      } catch (error) {
        this.clear();
        throw error;
      }
    }
    // 修复原因:不读取全局事件时,无界内存队列会随会话持续增长。
    const data = Buffer.from(JSON.stringify(event), 'utf8');
    const header = Buffer.allocUnsafe(4);
    header.writeUInt32BE(data.length);
    let position = this.writeOffset;
    for (const part of [header, data]) {
      let offset = 0;
      while (offset < part.length) {
        const written = writeSync(this.fd, part, offset, part.length - offset, position + offset);
        if (written === 0) throw new Error('全局事件缓冲写入失败');
        offset += written;
      }
      position += part.length;
    }
    this.writeOffset = position;
  }

  shift(): Event | undefined {
    if (this.empty || this.fd === undefined) return undefined;
    const header = this.read(4);
    const event = JSON.parse(this.read(header.readUInt32BE()).toString('utf8')) as Event;
    if (this.empty) this.clear();
    else if (this.readOffset >= 1024 * 1024 && this.readOffset >= this.writeOffset / 2) {
      try { this.compact(); } catch (error) { process.emitWarning(`全局事件缓冲回收失败: ${String(error)}`); }
    }
    return event;
  }

  private compact(): void {
    // 修复原因:队列持续有未读事件时永不排空,已读前缀会无限占用磁盘。
    const dir = mkdtempSync(join(tmpdir(), 'zhuye-events-'));
    let fd: number | undefined;
    try {
      fd = openSync(join(dir, 'events'), 'wx+', 0o600);
      let source = this.readOffset;
      let target = 0;
      const buffer = Buffer.allocUnsafe(65536);
      while (source < this.writeOffset) {
        const count = readSync(this.fd!, buffer, 0, Math.min(buffer.length, this.writeOffset - source), source);
        if (count === 0) throw new Error('全局事件缓冲读取失败');
        let written = 0;
        while (written < count) {
          const n = writeSync(fd, buffer, written, count - written, target + written);
          if (n === 0) throw new Error('全局事件缓冲写入失败');
          written += n;
        }
        source += count;
        target += count;
      }
      const oldFd = this.fd!;
      const oldDir = this.dir!;
      this.fd = fd;
      this.dir = dir;
      this.readOffset = 0;
      this.writeOffset = target;
      fd = undefined;
      closeSync(oldFd);
      rmSync(oldDir, { recursive: true, force: true });
    } catch (error) {
      if (fd !== undefined) {
        closeSync(fd);
        rmSync(dir, { recursive: true, force: true });
      }
      throw error;
    }
  }

  private read(size: number): Buffer {
    const data = Buffer.allocUnsafe(size);
    let offset = 0;
    while (offset < size) {
      const count = readSync(this.fd!, data, offset, size - offset, this.readOffset + offset);
      if (count === 0) throw new Error('全局事件缓冲读取失败');
      offset += count;
    }
    this.readOffset += size;
    return data;
  }

  clear(): void {
    activeSpools.delete(this);
    if (activeSpools.size === 0) process.removeListener('exit', cleanupSpools);
    if (this.fd !== undefined) closeSync(this.fd);
    if (this.dir !== undefined) rmSync(this.dir, { recursive: true, force: true });
    this.fd = undefined;
    this.dir = undefined;
    this.readOffset = this.writeOffset = 0;
  }
}
