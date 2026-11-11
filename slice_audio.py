''' Header : This file is responsible for importing audio in the project.'''


import os
import glob 
import shutil
import soundfile as sf


# Directories

Source = os.path.expanduser("~/.local/share/bolo/recordings")
destination = "./raw_recordings"

os.makedirs(destination, exist_ok = True) 

print(f"Scanning for audio files in : {Source}")


# Settings
Extention = ("*.wav", "*.mp3", "*.m4a", "*.flac", "*.ogg", "*.aac")

discovered_files = []
for ext in Extention:
    discovered_files.extend(glob.glob(os.path.join(Source, ext)))

# Keep only valid files (filter out directories)
discovered_files = [f for f in discovered_files if os.path.isfile(f)]
discovered_files.sort()

if not discovered_files:
    print(f"Aaayo! No audio files found in '{Source}'.")
    exit(1)

print(f"Found {len(discovered_files)} files.")

# Test with just 1 file first
files_to_process = discovered_files[:1]
print(f"Testing with {len(files_to_process)} file(s)...\n")

manifest = []
total_duration_sec = 0.0

for idx, src_path in enumerate(files_to_process, start=1):
    file_name = os.path.basename(src_path)
    file_size_mb = os.path.getsize(src_path) / (1024 * 1024)

    try: 
        info = sf.info(src_path)
        duration_sec = info.duration
        total_duration_sec += duration_sec

        dest_path = os.path.join(destination, file_name)
        shutil.copy2(src_path, dest_path)

        manifest.append({
            "name": file_name,
            "channels": info.channels,
            "format": info.format,
            "size_mb": file_size_mb,
        })
        
        print(f"Success!: Imported {file_name}") 
        print(f"{duration_sec:.1f} sec | {info.samplerate}Hz | {file_size_mb:.2f}mb")
        
    except Exception as e:   
        print(f"ERROR on {file_name}: {e}")

print("\n" + "=" * 50)
print(f"Total files Imported: {len(manifest)}")
print("=" * 50)

