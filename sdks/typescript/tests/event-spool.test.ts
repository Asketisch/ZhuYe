import { describe, expect, it } from 'vitest';
import { EventSpool } from '../src/event-spool.js';
import type { Event } from '../src/protocol.js';

describe('全局事件临时缓冲', () => {
  it('按顺序保留事件并在排空后可复用', () => {
    const spool = new EventSpool();
    const events = Array.from({ length: 500 }, (_, index) => ({
      id: '', msg: { type: 'future_event', index, payload: '中'.repeat(500) },
    } as Event));
    for (const event of events) spool.push(event);
    expect(events.map(() => spool.shift())).toEqual(events);
    expect(spool.empty).toBe(true);
    spool.push(events[0]);
    expect(spool.shift()).toEqual(events[0]);
  });

  it('队列未排空时回收已读前缀', () => {
    const spool = new EventSpool();
    const events = Array.from({ length: 120 }, (_, index) => ({
      id: '', msg: { type: 'future_event', index, payload: 'a'.repeat(20000) },
    } as Event));
    for (const event of events) spool.push(event);
    for (const event of events.slice(0, 90)) expect(spool.shift()).toEqual(event);
    expect((spool as unknown as { writeOffset: number }).writeOffset).toBeLessThan(1200000);
    expect(events.slice(90).map(() => spool.shift())).toEqual(events.slice(90));
  });
});
