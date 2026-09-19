/** Stable dictionary key: Unicode-normalized, whitespace-collapsed and case-folded. */
export function normalizePronunciationKey(value: string): string {
  return value.normalize('NFKC').replace(/[\u0591-\u05C7]/g, '').replace(/\s+/g, ' ').trim().toLocaleLowerCase('he-IL');
}

/**
 * Adds spoken context without attempting an error-prone full Hebrew number grammar.
 * Deepdub remains responsible for cardinal/ordinal inflection; these rewrites tell it
 * whether digits are money, a percentage, a time, a date, a phone, or an identifier.
 */
export function normalizeSpokenHebrew(text: string): string {
  let value = text.normalize('NFC');
  value = value.replace(/₪\s*(\d+(?:[.,]\d+)*)/g, '$1 שקלים');
  value = value.replace(/(\d+(?:[.,]\d+)*)\s*₪/g, '$1 שקלים');
  value = value.replace(/(\d+(?:[.,]\d+)?)\s*%/g, '$1 אחוזים');
  value = value.replace(/(?:טל(?:פון)?|נייד)\s*[:#-]?\s*(\+?\d[\d -]{6,}\d)/giu, (_match, digits: string) =>
    `מספר טלפון ${digits.replace(/\D/g, '').split('').join(' ')}`,
  );
  value = value.replace(/\b(?:הזמנה|order)\s*(?:מספר|#|no\.?|number)?\s*([A-Z0-9-]{4,})/giu, (_match, id: string) =>
    `מספר הזמנה ${id.replace(/[-_]/g, ' ').split('').join(' ')}`,
  );
  value = value.replace(/(?:בשעה\s*)?\b([01]?\d|2[0-3]):([0-5]\d)\b/g, 'בשעה $1 ו-$2 דקות');
  value = value.replace(/\b([0-3]?\d)[/.]([01]?\d)[/.](\d{2,4})\b/g, 'בתאריך $1 ל-$2 שנת $3');
  return value.replace(/\s+/g, ' ').trim();
}
