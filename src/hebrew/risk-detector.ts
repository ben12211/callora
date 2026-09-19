export interface HebrewRisk {
  risky: boolean;
  reasons: string[];
}

const HEBREW = /[\u0590-\u05FF]/u;
const LATIN = /[A-Za-z]/;

/** Deterministic, allocation-light gate used on the realtime path. */
export function detectHebrewPronunciationRisk(text: string, dictionaryHit = false): HebrewRisk {
  const reasons: string[] = [];
  if (dictionaryHit) reasons.push('dictionary');
  if (/\d/.test(text)) reasons.push('number');
  if (/[₪%]/.test(text)) reasons.push('currency-or-percent');
  if (/\b\d{1,2}:\d{2}\b/.test(text)) reasons.push('time');
  if (/\b\d{1,2}[/.]\d{1,2}[/.]\d{2,4}\b/.test(text)) reasons.push('date');
  if (/\+?\d[\d -]{6,}\d/.test(text)) reasons.push('phone-or-id');
  if (/[׳״'"]/.test(text)) reasons.push('geresh');
  if (HEBREW.test(text) && LATIN.test(text)) reasons.push('mixed-script');
  if (/\b[A-Z]{2,}\b/.test(text)) reasons.push('acronym');
  if (/\b(?:רח\.?|רחוב|ד"ר|עו"ד|בע"מ|מס\.?|ת\.ז\.)/u.test(text)) reasons.push('abbreviation');
  // Unvocalized Hebrew proper-name patterns are high-value G2P candidates.
  if (/\b(?:מר|גברת|דוקטור|רחוב|חברת|סניף)\s+[\u0590-\u05FF]{3,}/u.test(text)) reasons.push('name');
  return { risky: reasons.length > 0, reasons: [...new Set(reasons)] };
}

