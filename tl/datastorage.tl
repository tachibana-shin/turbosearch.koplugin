local record DataStorageModule
   getDataDir: function(any): string
   getSettingsDir: function(any): string
end

return {
   getDataDir = function(_self: any): string return "" end,
   getSettingsDir = function(_self: any): string return "" end,
} as DataStorageModule
