const fs = require('fs');
const path = require('path');

const DB_URL = 'https://raw.githubusercontent.com/mdqinc/SDL_GameControllerDB/master/gamecontrollerdb.txt';
const OUTPUT_DIR = 'dist/jsdb';

const VALID_MAPPINGS = new Set([
  'a', 'b', 'x', 'y', 'back', 'guide', 'start', 'leftstick', 'rightstick',
  'leftshoulder', 'rightshoulder', 'lefttrigger', 'righttrigger',
  'dpup', 'dpdown', 'dpleft', 'dpright',
  'leftx', 'lefty', 'rightx', 'righty'
]);

function parseSdlLine(line) {
  if (line.startsWith('#') || line.trim() === '') {
    return null;
  }

  const parts = line.split(',');
  const guid = parts[0];

  if (guid.length < 20) return null;

  const vendor = (guid.substring(10, 12) + guid.substring(8, 10)).toLowerCase();
  const product = (guid.substring(18, 20) + guid.substring(16, 18)).toLowerCase();
  const filename = `${vendor}-${product}.json`;

  const mapping = {};

  for (let i = 2; i < parts.length; i++) {
    const mappingPart = parts[i];
    if (!mappingPart.includes(':')) continue;

    const [sdlName, rawValue] = mappingPart.split(':');

    if (!VALID_MAPPINGS.has(sdlName)) {
      continue;
    }

    const typeChar = rawValue.charAt(0);

    if (typeChar === 'a' || typeChar === 'b') {
      const index = parseInt(rawValue.substring(1), 10);
      mapping[sdlName] = { type: typeChar === 'a' ? 'axis' : 'button', index: index };
    } else if (typeChar === 'h') {
      const hatParts = rawValue.substring(1).split('.');
      const index = parseInt(hatParts[0], 10);
      const mask = parseInt(hatParts[1], 10);
      mapping[sdlName] = { type: 'hat', index: index, mask: mask };
    }
  }

  if (Object.keys(mapping).length > 0) {
    return { filename, mapping };
  }

  return null;
}

async function main() {
  console.log(`Fetching controller DB from ${DB_URL}...`);

  let fileContent;
  try {
    const response = await fetch(DB_URL);
    if (!response.ok) {
      throw new Error(`Failed to fetch: ${response.status} ${response.statusText}`);
    }
    fileContent = await response.text();
    console.log('Successfully fetched controller DB.');
  } catch (error) {
    console.error('Error fetching game controller DB:', error);
    return;
  }
  
  console.log('Starting conversion...');

  if (!fs.existsSync(OUTPUT_DIR)) {
    fs.mkdirSync(OUTPUT_DIR, { recursive: true });
    console.log(`Created output directory: ${OUTPUT_DIR}`);
  }

  const lines = fileContent.split('\n');

  let convertedCount = 0;
  let skippedCount = 0;

  for (const line of lines) {
    const result = parseSdlLine(line);
    if (result) {
      const outputPath = path.join(OUTPUT_DIR, result.filename);
      const jsonContent = JSON.stringify(result.mapping, null, 2);
      fs.writeFileSync(outputPath, jsonContent);
      convertedCount++;
    } else {
      skippedCount++;
    }
  }

  console.log(`\nConversion complete!`);
  console.log(`  Successfully converted and wrote ${convertedCount} mapping files.`);
  console.log(`  Skipped ${skippedCount} lines (comments, empty, or invalid).`);
}

main();
