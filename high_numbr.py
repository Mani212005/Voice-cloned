a = list(map(int, input("Enter the Numbers separated by spaces: ").split()))

a.sort()

print("Array in increasing order: ",a)

largest = a[-1]

print("Largest number in the array:", largest)
