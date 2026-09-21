export interface HebrewRisk {
  risky: boolean;
  reasons: string[];
}

const HEBREW = /[\u0590-\u05FF]/u;
const LATIN = /[A-Za-z]/;
// `\b` only knows ASCII word characters, so it never fires between a space and a Hebrew
// letter. These lookarounds are the Hebrew-aware word edges.
const START = '(?<![\\p{L}\\p{N}])';
const ABBREVIATION = new RegExp(`${START}[בלמהושכ]?(?:רח\\.|רחוב|ד"ר|עו"ד|בע"מ|מס\\.|ת\\.ז\\.)`, 'u');
// Unvocalized Hebrew proper-name patterns are high-value G2P candidates. A one-letter
// prefix (ב, ל, מ, ה, ו, ש, כ) is allowed in front of the title word, and a geresh inside
// the name (ז'בוטינסקי) is part of it.
const NAME = new RegExp(`${START}[בלמהושכ]?(?:מר|גברת|דוקטור|רחוב|חברת|סניף)\\s+[\\u05D0-\\u05EA'׳]{3,}`, 'u');
const ACRONYM = new RegExp(`${START}[A-Z]{2,}(?![A-Za-z])`, 'u');

/** Deterministic, allocation-light gate used on the realtime path. */
export function detectHebrewPronunciationRisk(text: string, dictionaryHit = false): HebrewRisk {
  const reasons: string[] = [];
  if (dictionaryHit) reasons.push('dictionary');
  if (/\d/.test(text)) reasons.push('number');
  if (/[₪%]/.test(text)) reasons.push('currency-or-percent');
  if (/(?<!\d)\d{1,2}:\d{2}(?!\d)/.test(text)) reasons.push('time');
  if (/(?<!\d)\d{1,2}[/.]\d{1,2}[/.]\d{2,4}(?!\d)/.test(text)) reasons.push('date');
  if (/\+?\d[\d -]{6,}\d/.test(text)) reasons.push('phone-or-id');
  if (/[׳״'"]/.test(text)) reasons.push('geresh');
  if (HEBREW.test(text) && LATIN.test(text)) reasons.push('mixed-script');
  if (ACRONYM.test(text)) reasons.push('acronym');
  if (ABBREVIATION.test(text)) reasons.push('abbreviation');
  if (NAME.test(text)) reasons.push('name');
  return { risky: reasons.length > 0, reasons: [...new Set(reasons)] };
}

