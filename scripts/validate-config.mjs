import { readFile } from 'node:fs/promises';
import { parseDocument } from 'yaml';

const files = [
  '.github/workflows/ci-cd.yml',
  'docker-compose.yml',
  'docker-compose.prod.yml',
];

for (const file of files) {
  const document = parseDocument(await readFile(file, 'utf8'));
  if (document.errors.length > 0) {
    throw new Error(`${file}: ${document.errors.map((error) => error.message).join('; ')}`);
  }
}
