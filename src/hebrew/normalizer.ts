/** Stable dictionary key: Unicode-normalized, whitespace-collapsed and case-folded. */
export function normalizePronunciationKey(value: string): string {
  return value.normalize('NFKC').replace(/[\u0591-\u05C7]/g, '').replace(/\s+/g, ' ').trim().toLocaleLowerCase('he-IL');
}

const spellDigits = (value: string): string => value.replace(/\D/g, '').split('').join(' ');
// "09" is read as "zero nine"; the calendar and clock values below are spoken without it.
const dropLeadingZero = (value: string): string => String(Number(value));

// Israeli numbers: mobile 05X, landlines 02-09, 07X VoIP, 1-800/1-700, and +972.
const ISRAELI_PHONE = /(?<![\d\p{L}])(?:\+972[\s-]?|0)(?:5\d|7\d|[2-489])(?:[\s-]?\d){7}(?!\d)|(?<![\d\p{L}])1[\s-]?[78]00[\s-]?\d{2,3}[\s-]?\d{3,4}(?!\d)/gu;

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
    `מספר טלפון ${spellDigits(digits)}`,
  );
  // A bare number read as a quantity ("fifty, minus, one million...") is the worst
  // failure on a phone line, so any Israeli-shaped number is spelled digit by digit.
  value = value.replace(ISRAELI_PHONE, (digits: string) => spellDigits(digits));
  value = value.replace(/(?<!\p{L})(?:הזמנה|order)\s*(?:מספר|#|no\.?|number)?\s*([A-Z0-9-]{4,})/giu, (_match, id: string) =>
    `מספר הזמנה ${id.replace(/[-_]/g, ' ').split('').join(' ')}`,
  );
  value = value.replace(/(?:בשעה\s*)?(?<!\d)([01]?\d|2[0-3]):([0-5]\d)(?!\d)/g, (_match, hours: string, minutes: string) =>
    minutes === '00' ? `בשעה ${dropLeadingZero(hours)}` : `בשעה ${dropLeadingZero(hours)} ו-${dropLeadingZero(minutes)} דקות`,
  );
  // The preposition in front of a date ("ב-21/09") is absorbed; "בתאריך" already says it.
  value = value.replace(/(?<!\p{L})(?:ב-?|בתאריך\s*)?(?<!\d)([0-3]?\d)[/.]([01]?\d)[/.](\d{2,4})(?!\d)/gu, (_match, day: string, month: string, year: string) =>
    `בתאריך ${dropLeadingZero(day)} ל-${dropLeadingZero(month)} שנת ${year}`,
  );
  return value.replace(/\s+/g, ' ').trim();
}
