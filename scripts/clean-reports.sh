#!/bin/bash
# GetLatestRepo report cleanup script

# Default: clean reports older than 30 days
DAYS=${1:-30}

echo "=========================================="
echo "GetLatestRepo Report Cleanup"
echo "=========================================="
echo ""
echo "Cleaning reports older than $DAYS days..."
echo ""

cd "$(dirname "$0")/.."

# Count files before cleanup
BEFORE_COUNT=$(find reports/ -type f \( -name "*.html" -o -name "*.md" \) 2>/dev/null | wc -l)

echo "Reports before cleanup: $BEFORE_COUNT"

# Delete old files
find reports/ -type f \( -name "*.html" -o -name "*.md" \) -mtime +$DAYS -delete

# Remove empty directories
find reports/ -type d -empty -delete 2>/dev/null || true

# Count files after cleanup
AFTER_COUNT=$(find reports/ -type f \( -name "*.html" -o -name "*.md" \) 2>/dev/null | wc -l)
DELETED=$((BEFORE_COUNT - AFTER_COUNT))

echo "Reports after cleanup: $AFTER_COUNT"
echo "Deleted: $DELETED file(s)"
echo ""

if [ $DELETED -gt 0 ]; then
    echo "✓ Cleanup complete"
else
    echo "ℹ No old reports to clean up"
fi
