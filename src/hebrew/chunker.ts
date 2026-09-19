/**
 * Hebrew-aware streaming clause chunker. It favors early punctuation boundaries, but
 * protects decimals, dates, times, abbreviations, phone numbers, and mixed brand tokens.
 */
export class HebrewStreamingChunker {
  private pending = '';

  public constructor(
    private readonly minimumCharacters = 5,
    private readonly maximumCharacters = 110,
  ) {}

  public push(delta: string): string[] {
    this.pending += delta;
    return this.drain(false);
  }

  public finish(): string[] {
    return this.drain(true);
  }

  private drain(force: boolean): string[] {
    const ready: string[] = [];
    while (this.pending.trim()) {
      const boundary = this.findBoundary(force);
      if (boundary <= 0) break;
      const chunk = this.pending.slice(0, boundary);
      this.pending = this.pending.slice(boundary);
      if (chunk.trim()) ready.push(chunk);
      if (force && this.pending.length === 0) break;
    }
    return ready;
  }

  private findBoundary(force: boolean): number {
    if (force) return this.pending.length;
    for (let index = this.minimumCharacters; index < this.pending.length; index += 1) {
      const character = this.pending[index];
      if (!character || !/[,.!?;:،׃\n]/u.test(character)) continue;
      const before = this.pending.slice(Math.max(0, index - 12), index + 1);
      const after = this.pending.slice(index + 1, index + 14);
      if (/\d[.:/]$/u.test(before) && /^\d/u.test(after)) continue;
      if (/[A-Za-zא-ת]\.$/u.test(before) && /^\s?[A-Za-zא-ת]/u.test(after) && before.length < 5) continue;
      if (!/^\s|^$/u.test(after)) continue;
      return index + 1;
    }
    if (this.pending.length < this.maximumCharacters) return 0;
    const window = this.pending.slice(0, this.maximumCharacters + 1);
    const whitespace = window.lastIndexOf(' ');
    return whitespace >= this.minimumCharacters ? whitespace + 1 : this.maximumCharacters;
  }
}
