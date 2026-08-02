/**
 * Image suffix classification shared by backend attachment validation and
 * interactive clipboard/paste handling.
 *
 * Keep this leaf free of clipboard, process, native-module and image-decoder
 * imports. The direct StructuredIO runtime needs only the classification
 * predicate; importing imagePaste.ts for this constant pulled an otherwise
 * unreachable interactive clipboard implementation into the pure TUI graph.
 */
export const IMAGE_EXTENSION_REGEX = /\.(png|jpe?g|gif|webp)$/i
