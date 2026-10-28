/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   ExtraOptions.Infinity (FI_EO_BUILD, OD-125): the hook and ExtraOptions 0.3.2 are linked into one
   winmm.dll, and both trees define these external functions (the hook started as an ExtraOptions
   fork). native/extraoptions-infinity/build.py force-includes this header into every hook object
   only, so the hook keeps its own helpers, its own infinity.log and its own host checks, and
   ExtraOptions' sources stay unchanged. */
#ifndef FI_RENAME_H
#define FI_RENAME_H

#define azimuth_apply_livery_compatibility fi_azimuth_apply_livery_compatibility
#define azimuth_host_build fi_azimuth_host_build
#define azimuth_patch_host_code fi_azimuth_patch_host_code
#define azimuth_patch_host_codes fi_azimuth_patch_host_codes
#define azimuth_proxy_start fi_azimuth_proxy_start
#define float_is_finite fi_float_is_finite
#define log_append fi_log_append
#define log_append_value fi_log_append_value
#define log_reset fi_log_reset
#define log_set_directory fi_log_set_directory
#define log_start fi_log_start
#define memory_copy fi_memory_copy
#define memory_equal fi_memory_equal
#define memory_is_writable fi_memory_is_writable
#define memory_read fi_memory_read
#define memory_read_pointer fi_memory_read_pointer
#define path_join fi_path_join
#define sha256_finalize fi_sha256_finalize
#define sha256_initialize fi_sha256_initialize
#define sha256_update fi_sha256_update
#define status_append fi_status_append
#define status_get fi_status_get
#define status_set fi_status_set
#define text_equal_ignore_case fi_text_equal_ignore_case
#define text_find fi_text_find
#define text_find_unique fi_text_find_unique
#define text_length fi_text_length

#endif
